use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

// 定义全局异步上下文变量
tokio::task_local! {
    pub static OBFS_MODE: u8;
}

// 安全获取模式的辅助函数
#[inline]
pub fn get_obfs_mode() -> u8 {
    OBFS_MODE.try_with(|&m| m).unwrap_or(0)
}

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
            // 默认 8KB 缓冲区
            buf: vec![0; 8192].into_boxed_slice(),
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
                // 【核心修改点 1】：安全的 Chunking (随机裁剪)
                // 原理：在读取前缩小 ReadBuf 的容量，强制内核切片返回
                // ============================================================
                if max_len > 128 && get_obfs_mode() != 0 {
                    // 使用系统时间纳秒获取随机数，避免引入外部 rand 库
                    let nanos = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .subsec_nanos();
                    
                    // 生成 12 到 27 的随机百分比
                    // nanos % 16 会产生 0~15 的数。 12 + (0~15) = 12 ~ 27
                    let cut_percent = 12 + (nanos % 16) as usize; 
                    
                    // 计算需要裁剪掉的大小
                    let cut_size = (max_len * cut_percent) / 100;
                    
                    // 最终允许本次读取的最大目标大小
                    target_len = max_len - cut_size;
                }

                // 重点：仅将切分后的大小的 slice 传给 Tokio，数据绝对不会丢失
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
                            // 【核心修改点 2】：XOR 全局流量混淆
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
