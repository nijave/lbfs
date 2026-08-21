use crate::frame::{FrameHeader, HEADER_LEN};
use std::io::IoSlice;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug, thiserror::Error)]
pub enum IoError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol violation: {0}")]
    Protocol(&'static str),
}

pub async fn read_header<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<FrameHeader> {
    let mut buf = [0u8; HEADER_LEN];
    r.read_exact(&mut buf).await?;
    Ok(FrameHeader::decode(&buf))
}

pub async fn read_body<R: AsyncRead + Unpin>(
    r: &mut R,
    len: u32,
    max: u32,
) -> Result<Vec<u8>, IoError> {
    if len > max {
        return Err(IoError::Protocol("body_len exceeds negotiated maximum"));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await?;
    Ok(body)
}

pub async fn write_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    hdr: FrameHeader,
    body: &[u8],
    data: &[u8],
) -> std::io::Result<()> {
    debug_assert_eq!(hdr.body_len as usize, body.len());
    debug_assert_eq!(hdr.data_len as usize, data.len());
    let head = hdr.encode();
    let mut slices = [IoSlice::new(&head), IoSlice::new(body), IoSlice::new(data)];
    let mut total = HEADER_LEN + body.len() + data.len();
    let mut cursor = 0usize;
    // write_vectored can return short counts: advance across the slice list.
    while total > 0 {
        let n = w.write_vectored(&slices[..]).await?;
        if n == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        total -= n;
        cursor += n;
        if total > 0 {
            // Rebuild the slice window past `cursor` bytes.
            let (h, b) = (head.len(), body.len());
            let (hs, bs, ds) = (
                &head[cursor.min(h)..],
                &body[cursor.saturating_sub(h).min(b)..],
                &data[cursor.saturating_sub(h + b)..],
            );
            slices = [IoSlice::new(hs), IoSlice::new(bs), IoSlice::new(ds)];
        }
    }
    w.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::*;

    #[tokio::test]
    async fn frame_round_trips_over_duplex() {
        let (mut a, mut b) = tokio::io::duplex(1 << 16);
        let hdr = FrameHeader {
            request_id: 42,
            op_or_status: 16,
            flags: 0,
            body_len: 3,
            data_len: 4,
        };
        write_frame(&mut a, hdr, b"abc", b"wxyz").await.unwrap();
        let got = read_header(&mut b).await.unwrap();
        assert_eq!(got, hdr);
        let body = read_body(&mut b, got.body_len, MAX_BODY_SIZE)
            .await
            .unwrap();
        assert_eq!(body, b"abc");
        let mut data = vec![0u8; got.data_len as usize];
        tokio::io::AsyncReadExt::read_exact(&mut b, &mut data)
            .await
            .unwrap();
        assert_eq!(data, b"wxyz");
    }

    #[tokio::test]
    async fn partial_writes_preserve_frame_bytes() {
        // A 30-byte pipe forces short vectored writes that land mid-body and
        // mid-data, exercising the slice-window rebuild in `write_frame`.
        let (mut a, mut b) = tokio::io::duplex(30);
        let body: Vec<u8> = (0..=99u8).collect();
        let data: Vec<u8> = (0..1000u32)
            .map(|i| u8::try_from(i % 251).expect("modulus keeps value in u8 range"))
            .collect();
        let hdr = FrameHeader {
            request_id: 7,
            op_or_status: 16,
            flags: 0,
            body_len: u32::try_from(body.len()).unwrap(),
            data_len: u32::try_from(data.len()).unwrap(),
        };
        let (sent_body, sent_data) = (body.clone(), data.clone());
        let writer = tokio::spawn(async move {
            write_frame(&mut a, hdr, &sent_body, &sent_data)
                .await
                .unwrap();
        });

        let got = read_header(&mut b).await.unwrap();
        assert_eq!(got, hdr);
        let got_body = read_body(&mut b, got.body_len, MAX_BODY_SIZE)
            .await
            .unwrap();
        assert_eq!(got_body, body);
        let mut got_data = vec![0u8; got.data_len as usize];
        tokio::io::AsyncReadExt::read_exact(&mut b, &mut got_data)
            .await
            .unwrap();
        assert_eq!(got_data, data);
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn oversize_body_rejected_before_read() {
        let (mut a, mut b) = tokio::io::duplex(64);
        let hdr = FrameHeader {
            request_id: 1,
            op_or_status: 3,
            flags: 0,
            body_len: MAX_BODY_SIZE + 1,
            data_len: 0,
        };
        tokio::io::AsyncWriteExt::write_all(&mut a, &hdr.encode())
            .await
            .unwrap();
        let got = read_header(&mut b).await.unwrap();
        let err = read_body(&mut b, got.body_len, MAX_BODY_SIZE)
            .await
            .unwrap_err();
        assert!(matches!(err, IoError::Protocol(_)));
    }
}
