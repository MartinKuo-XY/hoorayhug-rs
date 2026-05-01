use std::io::Result;
use std::pin::Pin;
use std::task::{Poll, Context};

use tokio::io::ReadBuf;
use tokio::io::{AsyncRead, AsyncWrite};

use super::{CopyBuffer, AsyncIOBuf};
use super::bidi_copy_buf;

// ==============================================================
// Tokio 协程本地存储 (Task Local)
// 0: 原版普通转发 | 1: Client混淆客户端 | 2: Server混淆服务端
// ==============================================================
tokio::task_local! {
    pub static OBFS_MODE: u8;
}

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
        // ===============================================================
        // 【核心修改】：Chunking 随机碎包逻辑 (7% ~ 92%)
        // ===============================================================
        let max_len = self.buf.as_mut().len();
        let mut target_len = max_len;

        // 仅对较大的缓冲区进行切片，避免极小的数据包（如握手包）被过度碎化导致卡顿
        if max_len > 128 && get_obfs_mode() != 0 {
            // 1. 获取系统时间的纳秒级作为伪随机数种子 (极快，无需额外依赖 rand 库)
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos();
            
            // 2. 计算我们要“保留（读取）”的百分比：7% 到 92%
            // 算法解析：
            // - 最大值 92，最小值 7。 范围跨度 = 92 - 7 + 1 = 86
            // - 对 86 取模 (nanos % 86) 必然得到 0 到 85 之间的纯随机整数
            // - 加上底数 7：(0~85) + 7 = 7 到 92
            let read_percent = 7 + (nanos % 86) as usize; 
            
            // 3. 直接算出本次切片后允许读取的目标长度
            target_len = (max_len * read_percent) / 100;

            // 4. 安全兜底：如果算出来是 0 (比如 max_len=130，算出来7%是9.1，向下取整极少数情况出错时)
            // 强制最少读取 1 个字节，防止 TCP 状态机死锁（无限等待）
            if target_len == 0 {
                target_len = 1;
            }
        }

        // 用被随机截断后的目标长度创建 ReadBuf，欺骗内核只返回这么点数据
        let mut buf = ReadBuf::new(&mut self.buf.as_mut()[..target_len]);
        // ===============================================================
        
        match Pin::new(stream).poll_read(cx, &mut buf) {
            Poll::Ready(Ok(())) => {
                let len = buf.filled().len();
                
                // 动态检查：只要 mode 不是 0，就对本次读取的数据进行 XOR 混淆
                if len > 0 {
                    if get_obfs_mode() != 0 {
                        let slice = &mut self.buf.as_mut()[..len];
                        for byte in slice.iter_mut() {
                            *byte ^= 0x5A; // 核心 XOR 密钥
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
