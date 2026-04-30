use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

// ==========================================================
// 原版 Realm 必须的全局函数 (修复报错)
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

// ==========================================================
// 我们的混淆核心上下文
// ==========================================================
tokio::task_local! {
    pub static OBFS_MODE: u8;
}

// 安全获取模式的辅助函数
#[inline]
pub fn get_obfs_mode() -> u8 {
    OBFS_MODE.try_with(|&m| m).unwrap_or(0)
}

// ==========================================================
// 核心拷贝缓冲区逻辑 (加入 Chunking 和 XOR)
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
            // 动态读取全局设定的 buf_size
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

                // ============================================================
                // 【混淆特征】：安全的 Chunking (随机裁剪)
                // ============================================================
                if max_len > 128 && get_obfs_mode() != 0 {
                    // 使用纳秒获取伪随机数
                    let nanos = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .subsec_nanos();
                    
                    // nanos % 16 会产生 0~15。 12 + 0~15 = 12% ~ 27% 裁剪率
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

                            // ============================================================
                            // 【混淆特征】：XOR 混淆消除明文指纹
                            // ============================================================
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
// 原版 Realm 必须的双向拷贝封装 (修复 bidi_copy 找不到的报错)
// ==========================================================
struct Copy<'a, R: ?Sized, W: ?Sized> {
    reader: &'a mut R,
    writer: &'a mut W,
    buf: CopyBuffer,
}

impl<R, W> Future for Copy<'_, R, W>
where
    R: AsyncRead + Unpin + ?Sized,
    W: AsyncWrite + Unpin + ?Sized,
{
    type Output = io::Result<u64>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let me = &mut *self;
        me.buf.poll_copy(cx, Pin::new(&mut *me.reader), Pin::new(&mut *me.writer))
    }
}

pub async fn bidi_copy<A, B>(a: &mut A, b: &mut B) -> io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    let copy_a_to_b = Copy {
        reader: a,
        writer: b,
        buf: CopyBuffer::new(),
    };

    let copy_b_to_a = Copy {
        reader: b,
        writer: a,
        buf: CopyBuffer::new(),
    };

    // 并发执行 A->B 和 B->A 的流量转发
    tokio::try_join!(copy_a_to_b, copy_b_to_a)
}
