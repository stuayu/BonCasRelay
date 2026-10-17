use std::{
    io::{self, Read, Write},
    net::TcpStream,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
fn expired() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "frame deadline or server shutdown")
}
/// One deadline covers the entire frame, including its length byte.
pub fn transfer(
    stream: &mut TcpStream,
    data: &mut [u8],
    write: bool,
    deadline: Instant,
    stop: &AtomicBool,
) -> io::Result<()> {
    let mut offset = 0;
    while offset < data.len() {
        if stop.load(Ordering::Relaxed) || Instant::now() >= deadline {
            return Err(expired());
        }
        let timeout = Some(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(50))
                .max(Duration::from_millis(1)),
        );
        let result = if write {
            stream.set_write_timeout(timeout)?;
            stream.write(&data[offset..])
        } else {
            stream.set_read_timeout(timeout)?;
            stream.read(&mut data[offset..])
        };
        match result {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "closed stream",
                ))
            }
            Ok(n) => offset += n,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
pub fn read_frame(
    stream: &mut TcpStream,
    deadline: Instant,
    stop: &AtomicBool,
) -> io::Result<Vec<u8>> {
    let mut size = [0];
    transfer(stream, &mut size, false, deadline, stop)?;
    let mut body = vec![0; size[0] as usize];
    transfer(stream, &mut body, false, deadline, stop)?;
    Ok(body)
}
pub fn write_frame(
    stream: &mut TcpStream,
    body: &[u8],
    deadline: Instant,
    stop: &AtomicBool,
) -> io::Result<()> {
    let size = u8::try_from(body.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Response exceeds one-byte wire length",
        )
    })?;
    let mut frame = Vec::with_capacity(body.len() + 1);
    frame.push(size);
    frame.extend_from_slice(body);
    transfer(stream, &mut frame, true, deadline, stop)
}
