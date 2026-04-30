use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

// ==========================================================
// 全局配置与状态
// ==========================================================
static BUF_SIZE: AtomicUsize = AtomicUsize::new(8192);

#[inline]
pub fn set_buf_size(n: usize) {
    BUF_SIZE.store(n, Ordering::Relaxed);
}

#[inline]
pub fn buf_size() -> usize {
    BUF_SIZE.load(Ordering::Relaxed)
}

tokio::task_local! {
    pub static OBFS_MODE: u8;
}

#[inline]
pub fn get_obfs_mode() -> u8 {
    OBFS_MODE.try_with(|&m| m).unwrap_or(0)
}

// ==========================================================
// 核心拷贝缓冲区 (Chunking + XOR 混淆)
// ==========================================================
pub struct CopyBuffer {
    read_done: bool,
    need_flush: bool,
    pos: usize,
    cap: usize,
    amt: u64,
    buf: Box<[u8]>,
}

impl CopyBuffer {
    pub fn new() -> Self {
        Self {
            read_done: false,
            need_flush: false,
            pos: 0,
            cap: 0,
            amt: 0,
            buf: vec![0; buf_size()].into_boxed_slice(),
        }
    }

    pub fn poll_copy<R, W>(
        &mut self,
        cx: &mut Context<'_>,
        mut r: Pin<&mut R>,
        mut w: Pin<&mut W>,
    ) -> Poll<io::Result<u64>>
    where
        R: AsyncRead + ?Sized,
        W: AsyncWrite + ?Sized,
    {
        loop {
            // 1. 如果缓冲区空了，且没读完，就向内核读取数据
            if self.pos == self.cap && !self.read_done {
                let max_len = self.buf.len();
                let mut target_len = max_len;

                // 【混淆核心 1】：安全的随机碎包 (Chunking 12% ~ 27%)
                if max_len > 128 && get_obfs_mode() != 0 {
                    let nanos = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .subsec_nanos();
                    
                    let cut_percent = 12 + (nanos % 16) as usize; 
                    let cut_size = (max_len * cut_percent) / 100;
                    target_len = max_len - cut_size;
                }

                let mut read_buf = ReadBuf::new(&mut self.buf[..target_len]);
                
                match r.as_mut().poll_read(cx, &mut read_buf) {
                    Poll::Ready(Ok(())) => {
                        let n = read_buf.filled().len();
                        if n == 0 {
                            self.read_done = true;
                        } else {
                            self.pos = 0;
                            self.cap = n;

                            // 【混淆核心 2】：XOR 流量变异
                            if get_obfs_mode() != 0 {
                                let slice = &mut self.buf[..n];
                                for byte in slice.iter_mut() {
                                    *byte ^= 0x5A;
                                }
                            }
                        }
                    }
                    Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
                    Poll::Pending => {
                        if !self.need_flush {
                            return Poll::Pending;
                        }
                    }
                }
            }

            // 2. 如果缓冲区有数据，就往外写
            while self.pos < self.cap {
                match w.as_mut().poll_write(cx, &self.buf[self.pos..self.cap]) {
                    Poll::Ready(Ok(0)) => {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "write zero byte into writer",
                        )));
                    }
                    Poll::Ready(Ok(n)) => {
                        self.pos += n;
                        self.amt += n as u64;
                        self.need_flush = true;
                    }
                    Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
                    Poll::Pending => return Poll::Pending,
                }
            }

            // 3. 刷新数据
            if self.pos == self.cap && self.need_flush {
                match w.as_mut().poll_flush(cx) {
                    Poll::Ready(Ok(())) => {
                        self.need_flush = false;
                    }
                    Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
                    Poll::Pending => return Poll::Pending,
                }
            }

            // 4. 传输完成
            if self.read_done && self.pos == self.cap {
                return Poll::Ready(Ok(self.amt));
            }
        }
    }
}

// ==========================================================
// 修复借用检查：纯底层手写的双向拷贝状态机
// ==========================================================
struct BidiCopy<'a, A: ?Sized, B: ?Sized> {
    a: &'a mut A,
    b: &'a mut B,
    a_to_b: CopyBuffer,
    b_to_a: CopyBuffer,
    a_to_b_done: bool,
    b_to_a_done: bool,
}

impl<'a, A, B> Future for BidiCopy<'a, A, B>
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    type Output = io::Result<(u64, u64)>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let me = &mut *self;

        // 轮询 A -> B
        // 注意这里：借用在 poll_copy 调用结束后立刻释放，不会冲突
        if !me.a_to_b_done {
            match me.a_to_b.poll_copy(cx, Pin::new(&mut *me.a), Pin::new(&mut *me.b)) {
                Poll::Ready(Ok(_)) => me.a_to_b_done = true,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => {}
            }
        }

        // 轮询 B -> A
        if !me.b_to_a_done {
            match me.b_to_a.poll_copy(cx, Pin::new(&mut *me.b), Pin::new(&mut *me.a)) {
                Poll::Ready(Ok(_)) => me.b_to_a_done = true,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => {}
            }
        }

        // 只有双向都彻底结束，才返回 Ready
        if me.a_to_b_done && me.b_to_a_done {
            Poll::Ready(Ok((me.a_to_b.amt, me.b_to_a.amt)))
        } else {
            Poll::Pending
        }
    }
}

pub async fn bidi_copy<A, B>(a: &mut A, b: &mut B) -> io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    BidiCopy {
        a,
        b,
        a_to_b: CopyBuffer::new(),
        b_to_a: CopyBuffer::new(),
        a_to_b_done: false,
        b_to_a_done: false,
    }
    .await
}
