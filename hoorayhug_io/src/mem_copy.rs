use std::io::Result;
use std::pin::Pin;
use std::task::{Poll, Context};

use tokio::io::ReadBuf;
use tokio::io::{AsyncRead, AsyncWrite};

use super::{CopyBuffer, AsyncIOBuf};
use super::bidi_copy_buf;

// ==============================================================
// 终极杀器：Tokio 协程本地存储 (Task Local)
// 0: 原版普通转发 | 1: Client混淆客户端 | 2: Server混淆服务端
// ==============================================================
tokio::task_local! {
    pub static OBFS_MODE: u8;
}

/// 跨越函数栈，隔空安全获取当前连接的混淆模式
pub fn get_obfs_mode() -> u8 {
    OBFS_MODE.try_with(|x| *x).unwrap_or(0)
}
// ==============================================================

impl<B, SR, SW> AsyncIOBuf for CopyBuffer<B, SR, SW>
where
    B: AsMut<[u8]>,
    SR: AsyncRead + AsyncWrite + Unpin,
    SW: AsyncRead + AsyncWrite + Unpin,
{
    type StreamR = SR;
    type StreamW = SW;

    #[inline]
    fn poll_read_buf(&mut self, cx: &mut Context<'_>, stream: &mut Self::StreamR) -> Poll<Result<usize>> {
        let mut buf = ReadBuf::new(self.buf.as_mut());
        
        match Pin::new(stream).poll_read(cx, &mut buf) {
            Poll::Ready(Ok(())) => {
                let len = buf.filled().len();
                
                // 动态检查：只要 mode 不是 0，就对数据进行 XOR
                if len > 0 {
                    if get_obfs_mode() != 0 {
                        let slice = &mut self.buf.as_mut()[..len];
                        for byte in slice.iter_mut() {
                            *byte ^= 0x5A; // XOR 密钥
                        }
                    }
                }
                
                Poll::Ready(Ok(len))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }

    #[inline]
    fn poll_write_buf(&mut self, cx: &mut Context<'_>, stream: &mut Self::StreamW) -> Poll<Result<usize>> {
        Pin::new(stream).poll_write(cx, &self.buf.as_mut()[self.pos..self.cap])
    }

    #[inline]
    fn poll_flush_buf(&mut self, cx: &mut Context<'_>, stream: &mut Self::StreamW) -> Poll<Result<()>> {
        Pin::new(stream).poll_flush(cx)
    }
}

pub async fn bidi_copy<A, B>(a: &mut A, b: &mut B) -> Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let a_to_b_buf = CopyBuffer::new(vec![0u8; buf_size()].into_boxed_slice());
    let b_to_a_buf = CopyBuffer::new(vec![0u8; buf_size()].into_boxed_slice());
    bidi_copy_buf(a, b, a_to_b_buf, b_to_a_buf).await
}

mod buf_ctl {
    pub const DF_BUF_SIZE: usize = 0x2000;
    static mut BUF_SIZE: usize = DF_BUF_SIZE;
    #[inline]
    pub fn buf_size() -> usize { unsafe { BUF_SIZE } }
    #[inline]
    pub fn set_buf_size(n: usize) { unsafe { BUF_SIZE = n } }
}

pub use buf_ctl::{buf_size, set_buf_size};
