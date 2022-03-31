use std::future::Future;
use std::time::Duration;

use bytes::Buf;
use futures_util::FutureExt;
use poem::{http::StatusCode, Result};
use poem_wasm::ffi::{
    RawEvent, RawSubscription, ERRNO_OK, ERRNO_UNKNOWN, ERRNO_WOULD_BLOCK,
    SUBSCRIPTION_TYPE_REQUEST_READ, SUBSCRIPTION_TYPE_RESPONSE_WRITE, SUBSCRIPTION_TYPE_TIMEOUT,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wasmtime::{AsContextMut, Caller, Extern, Linker, Memory, Trap};

use crate::{state::WasmEndpointState, WasmHandlerError};

pub(crate) fn add_to_linker<State>(linker: &mut Linker<WasmEndpointState<State>>) -> Result<()>
where
    State: Send + 'static,
{
    linker.func_wrap("poem", "read_request", read_request)?;
    linker.func_wrap("poem", "read_request_body", read_request_body)?;
    linker.func_wrap("poem", "send_response", send_response)?;
    linker.func_wrap("poem", "write_response_body", write_response_body)?;
    linker.func_wrap3_async("poem", "poll", poll)?;

    Ok(())
}

fn get_memory<T>(caller: &mut Caller<'_, T>) -> Result<Memory, WasmHandlerError> {
    if let Some(Extern::Memory(memory)) = caller.get_export("memory") {
        Ok(memory)
    } else {
        Err(WasmHandlerError::MemoryNotFound)
    }
}

#[inline]
fn get_memory_slice_mut(
    memory: &mut [u8],
    offset: u32,
    len: u32,
) -> Result<&mut [u8], WasmHandlerError> {
    memory
        .get_mut(offset as usize..(offset + len) as usize)
        .ok_or_else(|| WasmHandlerError::MemoryAccess)
}

#[inline]
fn set_ret_len(memory: &mut [u8], buf: u32, ret_len: u32) -> Result<(), WasmHandlerError> {
    get_memory_slice_mut(memory, buf, 4)?.copy_from_slice(&ret_len.to_le_bytes());
    Ok(())
}

fn read_request<State>(
    mut caller: Caller<'_, WasmEndpointState<State>>,
    buf: u32,
    buf_len: u32,
    request_len: u32,
) -> Result<(), Trap> {
    let memory = get_memory(&mut caller)?;
    let (memory, state) = memory.data_and_store_mut(caller.as_context_mut());

    if buf_len < state.request.len() as u32 {
        set_ret_len(memory, request_len, state.request.len() as u32)?;
        return Ok(());
    }

    get_memory_slice_mut(memory, buf, state.request.len() as u32)?
        .copy_from_slice(state.request.as_bytes());
    set_ret_len(memory, request_len, state.request.len() as u32)?;
    Ok(())
}

fn read_request_body<State>(
    mut caller: Caller<'_, WasmEndpointState<State>>,
    buf: u32,
    buf_len: u32,
    bytes_read: u32,
) -> Result<i32, Trap> {
    let memory = get_memory(&mut caller)?;
    let (memory, state) = memory.data_and_store_mut(caller.as_context_mut());

    if state.request_body_eof {
        set_ret_len(memory, bytes_read, 0)?;
        return Ok(ERRNO_OK);
    }

    if !state.request_body_buf.has_remaining() {
        return Ok(ERRNO_WOULD_BLOCK);
    }

    let sz = buf_len.min(state.request_body_buf.remaining() as u32);
    get_memory_slice_mut(memory, buf, sz)?
        .copy_from_slice(&state.request_body_buf.split_to(sz as usize));
    set_ret_len(memory, bytes_read, sz)?;
    Ok(0)
}

fn send_response<State>(
    mut caller: Caller<'_, WasmEndpointState<State>>,
    status: u32,
    headers_buf: u32,
    headers_buf_len: u32,
) -> Result<(), Trap> {
    let memory = get_memory(&mut caller)?;
    let (memory, state) = memory.data_and_store_mut(caller.as_context_mut());

    if status > u16::MAX as u32 {
        return Err(WasmHandlerError::InvalidStatusCode.into());
    }

    let status =
        StatusCode::from_u16(status as u16).map_err(|_| WasmHandlerError::InvalidStatusCode)?;
    let data = get_memory_slice_mut(memory, headers_buf, headers_buf_len)?;
    let data = std::str::from_utf8(data).expect("valid header map");
    let headers = poem_wasm::decode_headers(data);
    let _ = state.response_sender.send((status, headers));
    Ok(())
}

fn write_response_body<State>(
    mut caller: Caller<'_, WasmEndpointState<State>>,
    data: u32,
    data_len: u32,
    bytes_written: u32,
) -> Result<i32, Trap> {
    let memory = get_memory(&mut caller)?;
    let (memory, state) = memory.data_and_store_mut(caller.as_context_mut());

    let data = get_memory_slice_mut(memory, data, data_len)?;
    let sz = data_len.min(4096 - state.response_body_buf.len() as u32);

    if sz == 0 {
        return Ok(ERRNO_WOULD_BLOCK);
    }
    state
        .response_body_buf
        .extend_from_slice(&data[..sz as usize]);
    set_ret_len(memory, bytes_written, sz)?;
    Ok(ERRNO_OK)
}

fn poll<'a, State: Send>(
    mut caller: Caller<'a, WasmEndpointState<State>>,
    subscriptions: u32,
    num_subscriptions: u32,
    event: u32,
) -> Box<dyn Future<Output = Result<(), Trap>> + Send + 'a> {
    Box::new(async move {
        let memory = get_memory(&mut caller)?;
        let (memory, state) = memory.data_and_store_mut(caller.as_context_mut());

        if num_subscriptions == 0 {
            return Err(WasmHandlerError::NoSubscriptions.into());
        }

        unsafe {
            let subscriptions = std::slice::from_raw_parts(
                memory.as_ptr().add(subscriptions as usize) as *const RawSubscription,
                num_subscriptions as usize,
            );

            let WasmEndpointState {
                request_body_reader,
                response_body_writer,
                request_body_buf,
                response_body_buf,
                request_body_eof,
                ..
            } = state;
            let mut futures = Vec::new();

            if let Some(item) = subscriptions
                .iter()
                .find(|item| item.ty == SUBSCRIPTION_TYPE_REQUEST_READ)
            {
                let userdata = item.userdata;
                futures.push(
                    async move {
                        let mut data = [0; 4096];

                        match request_body_reader.read(&mut data).await {
                            Ok(0) => {
                                *request_body_eof = true;
                                RawEvent {
                                    ty: SUBSCRIPTION_TYPE_REQUEST_READ,
                                    userdata,
                                    errno: ERRNO_OK,
                                }
                            }
                            Ok(sz) => {
                                request_body_buf.extend_from_slice(&data[..sz]);
                                RawEvent {
                                    ty: SUBSCRIPTION_TYPE_REQUEST_READ,
                                    userdata,
                                    errno: ERRNO_OK,
                                }
                            }
                            Err(_) => RawEvent {
                                ty: SUBSCRIPTION_TYPE_REQUEST_READ,
                                userdata,
                                errno: ERRNO_UNKNOWN,
                            },
                        }
                    }
                    .boxed(),
                );
            }

            if let Some(item) = subscriptions
                .iter()
                .find(|item| item.ty == SUBSCRIPTION_TYPE_RESPONSE_WRITE)
            {
                let userdata = item.userdata;
                futures.push(
                    async move {
                        match response_body_writer.write(&response_body_buf).await {
                            Ok(sz) => {
                                response_body_buf.advance(sz);
                                RawEvent {
                                    ty: SUBSCRIPTION_TYPE_RESPONSE_WRITE,
                                    userdata,
                                    errno: ERRNO_OK,
                                }
                            }
                            Err(_) => RawEvent {
                                ty: SUBSCRIPTION_TYPE_RESPONSE_WRITE,
                                userdata,
                                errno: ERRNO_UNKNOWN,
                            },
                        }
                    }
                    .boxed(),
                );
            }

            for subscription in subscriptions {
                let userdata = subscription.userdata;

                if subscription.ty == SUBSCRIPTION_TYPE_TIMEOUT {
                    futures.push(
                        async move {
                            tokio::time::sleep(Duration::from_millis(subscription.timeout as u64))
                                .await;
                            RawEvent {
                                ty: SUBSCRIPTION_TYPE_TIMEOUT,
                                userdata,
                                errno: ERRNO_OK,
                            }
                        }
                        .boxed(),
                    );
                }
            }

            Ok(
                *(memory.as_mut_ptr().add(event as usize) as *mut RawEvent) =
                    futures_util::future::select_all(futures).await.0,
            )
        }
    })
}