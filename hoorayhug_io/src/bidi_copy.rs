use std::io::Result;
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use std::future::Future;

use tokio::io::{AsyncRead, AsyncWrite, AsyncReadExt, AsyncWriteExt};

use super::{AsyncIOBuf, CopyBuffer};
use super::mem_copy::get_obfs_mode;

enum TransferState<B, SR, SW> {
    Running(CopyBuffer<B, SR, SW>),
    ShuttingDown(u64),
    Done(u64),
}

fn transfer<B, SL, SR>(
    cx: &mut Context<'_>,
    state: &mut TransferState<B, SL, SR>,
    r: &mut <CopyBuffer<B, SL, SR> as AsyncIOBuf>::StreamR,
    w: &mut <CopyBuffer<B, SL, SR> as AsyncIOBuf>::StreamW,
) -> Poll<Result<u64>>
where
    B: Unpin,
    SL: AsyncRead + AsyncWrite + Unpin,
    SR: AsyncRead + AsyncWrite + Unpin,
    CopyBuffer<B, SL, SR>: AsyncIOBuf,
    CopyBuffer<B, SR, SL>: AsyncIOBuf,
{
    loop {
        match state {
            TransferState::Running(buf) => {
                let count = ready!(buf.poll_copy(cx, r, w))?;
                *state = TransferState::ShuttingDown(count);
            }
            TransferState::ShuttingDown(count) => {
                ready!(Pin::new(&mut *w).poll_shutdown(cx))?;
                *state = TransferState::Done(*count);
            }
            TransferState::Done(count) => return Poll::Ready(Ok(*count)),
        }
    }
}

fn transfer2<B, SL, SR>(
    cx: &mut Context<'_>,
    state: &mut TransferState<B, SR, SL>,
    r: &mut <CopyBuffer<B, SL, SR> as AsyncIOBuf>::StreamW,
    w: &mut <CopyBuffer<B, SL, SR> as AsyncIOBuf>::StreamR,
) -> Poll<Result<u64>>
where
    B: Unpin,
    SL: AsyncRead + AsyncWrite + Unpin,
    SR: AsyncRead + AsyncWrite + Unpin,
    CopyBuffer<B, SL, SR>: AsyncIOBuf,
    CopyBuffer<B, SR, SL>: AsyncIOBuf,
{
    let r: &mut <CopyBuffer<B, SR, SL> as AsyncIOBuf>::StreamR = unsafe { std::mem::transmute(r) };
    let w: &mut <CopyBuffer<B, SR, SL> as AsyncIOBuf>::StreamW = unsafe { std::mem::transmute(w) };
    loop {
        match state {
            TransferState::Running(buf) => {
                let count = ready!(buf.poll_copy(cx, r, w))?;
                *state = TransferState::ShuttingDown(count);
            }
            TransferState::ShuttingDown(count) => {
                ready!(Pin::new(&mut *w).poll_shutdown(cx))?;
                *state = TransferState::Done(*count);
            }
            TransferState::Done(count) => return Poll::Ready(Ok(*count)),
        }
    }
}

struct BidiCopy<'a, B, SL, SR>
where
    B: Unpin,
    SL: AsyncRead + AsyncWrite + Unpin,
    SR: AsyncRead + AsyncWrite + Unpin,
    CopyBuffer<B, SL, SR>: AsyncIOBuf,
    CopyBuffer<B, SR, SL>: AsyncIOBuf,
{
    a: &'a mut <CopyBuffer<B, SL, SR> as AsyncIOBuf>::StreamR,
    b: &'a mut <CopyBuffer<B, SL, SR> as AsyncIOBuf>::StreamW,
    a_to_b: TransferState<B, SL, SR>,
    b_to_a: TransferState<B, SR, SL>,
}

impl<'a, B, SL, SR> Future for BidiCopy<'a, B, SL, SR>
where
    B: Unpin,
    SL: AsyncRead + AsyncWrite + Unpin,
    SR: AsyncRead + AsyncWrite + Unpin,
    CopyBuffer<B, SL, SR>: AsyncIOBuf,
    CopyBuffer<B, SR, SL>: AsyncIOBuf,
{
    type Output = Result<(u64, u64)>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let BidiCopy { a, b, a_to_b, b_to_a } = self.get_mut();

        let a_to_b = transfer(cx, a_to_b, a, b)?;
        let b_to_a = transfer2::<B, SL, SR>(cx, b_to_a, b, a)?;

        #[cfg(not(feature = "brutal-shutdown"))]
        {
            let a_to_b = ready!(a_to_b);
            let b_to_a = ready!(b_to_a);
            Poll::Ready(Ok((a_to_b, b_to_a)))
        }

        #[cfg(feature = "brutal-shutdown")]
        {
            match (a_to_b, b_to_a) {
                (Poll::Ready(a), Poll::Ready(b)) => Poll::Ready(Ok((a, b))),
                (Poll::Pending, Poll::Ready(b)) => Poll::Ready(Ok((0, b))),
                (Poll::Ready(a), Poll::Pending) => Poll::Ready(Ok((a, 0))),
                _ => Poll::Pending,
            }
        }
    }
}

pub async fn bidi_copy_buf<B, SR, SW>(
    a: &mut <CopyBuffer<B, SR, SW> as AsyncIOBuf>::StreamR,
    b: &mut <CopyBuffer<B, SR, SW> as AsyncIOBuf>::StreamW,
    a_to_b_buf: CopyBuffer<B, SR, SW>,
    b_to_a_buf: CopyBuffer<B, SW, SR>,
) -> Result<(u64, u64)>
where
    B: Unpin,
    SR: AsyncRead + AsyncWrite + Unpin,
    SW: AsyncRead + AsyncWrite + Unpin,
    CopyBuffer<B, SR, SW>: AsyncIOBuf,
    CopyBuffer<B, SW, SR>: AsyncIOBuf,
{
    let a_stream: &mut SR = unsafe { std::mem::transmute::<_, &mut SR>(&mut *a) };
    let b_stream: &mut SW = unsafe { std::mem::transmute::<_, &mut SW>(&mut *b) };

    // ==============================================================
    // 动态获取当前配置，按规则执行首包操作
    // ==============================================================
    match get_obfs_mode() {
        1 => { // Client 模式：发往国外的连接注入垃圾数据
            let mut pad = [0u8; 26];
            let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos();
            for i in 0..26 { pad[i] = ((t >> (i % 8)) & 0xFF) as u8; }
            b_stream.write_all(&pad).await?;
            b_stream.flush().await?;
        }
        2 => { // Server 模式：从国内来的连接吃掉垃圾数据
            let mut pad = [0u8; 26];
            a_stream.read_exact(&mut pad).await?;
        }
        _ => {} // 0 = None，不注入也不吃数据
    }

    let a_to_b = TransferState::Running(a_to_b_buf);
    let b_to_a = TransferState::Running(b_to_a_buf);

    BidiCopy { a, b, a_to_b, b_to_a }.await
}
