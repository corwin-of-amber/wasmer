use js_sys::{ArrayBuffer, DataView, Uint8Array};

pub struct Chunked {
    abuf: ArrayBuffer,
    chunk_size: usize
}

impl Chunked {
    pub fn new(abuf: ArrayBuffer, chunk_size: usize) -> Self {
        Chunked { abuf, chunk_size }
    }

    #[inline]
    fn data_view(&self) -> DataView {
        DataView::new(&self.abuf, 0, self.abuf.byte_length() as usize)
    }

    fn address(&self, chunk: usize, offset: usize) -> usize {
        chunk * self.chunk_size + offset
    }

    pub fn read_u32(&self, chunk: usize, index: usize) -> u32 {
        self.data_view().get_uint32(self.address(chunk, 4 * index))
    }

    pub fn write_u32(&self, chunk: usize, index: usize, val: u32) {
        self.data_view().set_uint32(self.address(chunk, 4 * index), val);
    }

    #[allow(dead_code)]
    pub fn read_bytes(&self, chunk: usize, byte_offset: usize, byte_length: usize) -> Vec<u8> {
        assert!(byte_offset < self.chunk_size);
        let byte_length = byte_length.min(self.chunk_size - byte_offset);
        Uint8Array::new_with_byte_offset_and_length
            (&self.abuf, self.address(chunk, byte_offset) as u32, byte_length as u32).to_vec()
    }

    pub fn read_bytes_into(&self, chunk: usize, byte_offset: usize, out: &mut [u8]) -> usize {
        assert!(byte_offset < self.chunk_size);
        let byte_length = out.len().min(self.chunk_size - byte_offset);
        Uint8Array::new_with_byte_offset_and_length
            (&self.abuf, self.address(chunk, byte_offset) as u32, byte_length as u32)
            .copy_to(&mut out[..byte_length]);
        byte_length
    }

    pub fn write_bytes(&self, chunk: usize, byte_offset: usize, data: &[u8]) -> usize {
        assert!(byte_offset < self.chunk_size);
        let byte_length = data.len().min(self.chunk_size - byte_offset);
        Uint8Array::new_with_byte_offset_and_length
            (&self.abuf, self.address(chunk, byte_offset) as u32, byte_length as u32)
            .copy_from(&data[..byte_length]);
        byte_length
    }
}

pub struct Ropes {
    storage: Chunked,
    next_free: usize
}

impl Ropes {

    pub fn new(storage: Chunked) -> Self {
        Ropes { storage, next_free: 1 }
    }

    pub fn contains_key(&self, key: usize) -> bool {
        self.storage.read_u32(key, 0) != 0
    }

    pub fn alloc(&mut self) -> usize {
        let key = self.alloc_peek();
        self.next_free += 1;
        key
    }

    /** @note there is risk of race if `alloc_peek()` is called and then `alloc()`.
     * It cannot be assumed that they would return the same value in the presence of
     * multiple concurrent writers.
     */
    pub fn alloc_peek(&mut self) -> usize {
        while self.contains_key(self.next_free) {
            self.next_free += 1;
        }
        self.next_free
    }

    #[allow(dead_code)]
    pub fn insert(&mut self, data: &[u8]) -> (usize, u32) {
        let key = self.alloc();
        (key, self.insert_at(key, data))
    }

    pub fn insert_at(&mut self, key: usize, data: &[u8]) -> u32 {
        let ver: u32 = self.storage.read_u32(key, 0) + 1;
        self.storage.write_u32(key, 0, ver);
        self.storage.write_u32(key, 1, data.len() as u32);
        let nxt_ = self.storage.read_u32(key, 2);
        let wr_len = self.storage.write_bytes(key, 4 * 3, data);
        let nxt =
            if wr_len < data.len() { self.insert_cont(nxt_, &data[wr_len..]) }
            else { 0 } as u32;
        if nxt != nxt_ { self.storage.write_u32(key, 2, nxt); }
        ver
    }

    fn insert_cont(&mut self, maybe_at: u32, data: &[u8]) -> usize {
        let head = if maybe_at > 1 { maybe_at as usize } else { self.alloc() };
        let mut offset = 0;
        let mut len = data.len();
        let mut cur = head;
        while len > 0 {
            let nxt_ = self.storage.read_u32(cur, 0);
            let wr_len =
                self.storage.write_bytes(cur, 4 * 1, &data[offset..]);
            offset += wr_len;
            len -= wr_len;
            let nxt =
                if len > 0 { if nxt_ > 1 { nxt_ } else { self.alloc() as u32 } }
                else { 1 /* not 0 so that we don't mistake this for an empty block */ };
            if nxt != nxt_ { self.storage.write_u32(cur, 0, nxt); }
            cur = nxt as usize;
        }
        head
    }

    /*
     * recursive version that fails because of stack overflow:

    fn insert_cont(&mut self, maybe_at: u32, data: &[u8]) -> usize {
        let key = if maybe_at > 1 { maybe_at as usize } else { self.alloc() };
        self.insert_cont_at(key, data);
        key
    }

    fn insert_cont_at(&mut self, key: usize, data: &[u8]) {
        let nxt_ = self.storage.read_u32(key, 0);
        let wr_len = self.storage.write_bytes(key, 4 * 1, data);
        let nxt =
            if wr_len < data.len() { self.insert_cont(nxt_, &data[wr_len..]) }
            else { 1 } as u32;
        if nxt != nxt_ { self.storage.write_u32(key, 0, nxt); }
    }*/

    pub fn ver_peek(&self, key: usize) -> u32 {
        self.storage.read_u32(key, 0)
    }

    /** this can also have a simpler recursive version */
    pub fn get(&self, key: usize) -> Vec<u8> {
        let mut len = self.storage.read_u32(key, 1) as usize;
        let mut vec: Vec<u8> = Vec::with_capacity(len);
        unsafe { vec.set_len(len) };  // uninitialized!
        let mut offset: usize = 0;
        let mut cur = key;
        let mut offset_data = 4 * 3;
        let mut index_nxt = 2;
        while len > 0 {
            let rd_cnt =
                self.storage.read_bytes_into(cur, offset_data, &mut vec[offset..]);
            len -= rd_cnt;
            offset += rd_cnt;
            if len > 0 {
                cur = self.storage.read_u32(cur, index_nxt) as usize;
                offset_data = 4 * 1;
                index_nxt = 0;
            }
        }
        assert!(offset == vec.len()); // fully initialized!
        vec
    }
}
