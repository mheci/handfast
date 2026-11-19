use bytes::Bytes;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::error::Error;

pub(crate) fn unix(time: SystemTime) -> u32 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs() as u32,
        Err(error) => {
            let duration = error.duration();
            // Unix timestamps round toward negative infinity before casting.
            (0u32)
                .wrapping_sub(duration.as_secs() as u32)
                .wrapping_sub(u32::from(duration.subsec_nanos() != 0))
        }
    }
}

pub(crate) async fn read_packet<S: AsyncRead + Unpin>(
    stream: &mut S,
    max_length: u32,
) -> Result<Bytes, Error> {
    let length = stream.read_u32().await?;
    if length > max_length {
        return Err(Error::BadMessage("packet length limit exceeded".to_owned()));
    }

    let mut buf = vec![0; length as usize];
    stream.read_exact(&mut buf).await?;

    Ok(Bytes::from(buf))
}
