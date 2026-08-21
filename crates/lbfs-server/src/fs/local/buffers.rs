use std::sync::{Arc, Mutex};

struct Inner {
    bufs: Mutex<Vec<Box<[u8]>>>,
    buf_size: usize,
    max_pooled: usize,
}

#[derive(Clone)]
pub struct BufferPool(Arc<Inner>);

impl BufferPool {
    pub fn new(buf_size: usize, max_pooled: usize) -> BufferPool {
        BufferPool(Arc::new(Inner {
            bufs: Mutex::new(Vec::new()),
            buf_size,
            max_pooled,
        }))
    }

    pub fn get(&self) -> PooledBuf {
        let buf = self
            .0
            .bufs
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| vec![0u8; self.0.buf_size].into_boxed_slice());
        PooledBuf {
            buf: Some(buf),
            len: 0,
            pool: Arc::downgrade(&self.0),
        }
    }

    #[cfg(test)]
    pub fn pooled_count(&self) -> usize {
        self.0.bufs.lock().unwrap().len()
    }
}

pub struct PooledBuf {
    buf: Option<Box<[u8]>>,
    len: usize,
    pool: std::sync::Weak<Inner>,
}

impl PooledBuf {
    pub fn as_slice(&self) -> &[u8] {
        &self.buf.as_ref().unwrap()[..self.len]
    }
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        self.buf.as_mut().unwrap()
    }
    pub fn set_len(&mut self, n: usize) {
        assert!(n <= self.capacity());
        self.len = n;
    }
    pub fn capacity(&self) -> usize {
        self.buf.as_ref().unwrap().len()
    }
}

impl Drop for PooledBuf {
    fn drop(&mut self) {
        if let (Some(buf), Some(inner)) = (self.buf.take(), self.pool.upgrade()) {
            let mut bufs = inner.bufs.lock().unwrap();
            if bufs.len() < inner.max_pooled {
                bufs.push(buf);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_use_drop_reuses_storage() {
        let pool = BufferPool::new(4096, 2);
        let mut a = pool.get();
        a.as_mut_slice()[0] = 7;
        a.set_len(1);
        assert_eq!(a.as_slice(), &[7]);
        let ptr = a.as_slice().as_ptr();
        drop(a);
        assert_eq!(pool.pooled_count(), 1); // drop returned it to the pool
        let b = pool.get();
        assert_eq!(pool.pooled_count(), 0); // get took that one, did not allocate
        assert_eq!(b.as_slice().as_ptr(), ptr); // same storage came back
        assert_eq!(b.as_slice().len(), 0); // but length reset
    }

    #[test]
    fn pool_caps_retained_buffers() {
        let pool = BufferPool::new(64, 1);
        let a = pool.get();
        let b = pool.get();
        drop(a);
        drop(b); // second drop discards, no growth beyond max_pooled
        assert_eq!(pool.pooled_count(), 1);
    }
}
