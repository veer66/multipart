// Copyright 2016 `multipart` Crate Developers
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

//! Boundary parsing for `multipart` requests.

use ::safemem;

use super::buf_redux::BufReader;
use super::twoway;

use log::LogLevel;

use std::cmp;
use std::borrow::Borrow;

use std::io;
use std::io::prelude::*;

/// A struct implementing `Read` and `BufRead` that will yield bytes until it sees a given sequence.
#[derive(Debug)]
pub struct BoundaryReader<R> {
    source: BufReader<R>,
    boundary: Vec<u8>,
    search_idx: usize,
    boundary_read: bool,
    at_end: bool,
}

impl<R> BoundaryReader<R> where R: Read {
    #[doc(hidden)]
    pub fn from_reader<B: Into<Vec<u8>>>(reader: R, boundary: B) -> BoundaryReader<R> {
        let mut boundary = boundary.into();
        safemem::prepend(b"--", &mut boundary);

        BoundaryReader {
            source: BufReader::new(reader),
            boundary: boundary,
            search_idx: 0,
            boundary_read: false,
            at_end: false,
        }
    }

    fn read_to_boundary(&mut self) -> io::Result<&[u8]> {
        // Make sure there's enough bytes in the buffer to positively identify the boundary.
        let min_len = self.search_idx + (self.boundary.len() * 2);

        let buf = fill_buf_min(&mut self.source, min_len)?;

        if buf.is_empty() {
            debug!("fill_buf_min returned zero-sized buf");
            self.at_end = true;
            return Ok(buf);
        }

        trace!("Buf: {:?}", String::from_utf8_lossy(buf));

        debug!("Before-loop Buf len: {} Search idx: {} Boundary read: {:?}",
               buf.len(), self.search_idx, self.boundary_read);

        if !self.boundary_read && self.search_idx < buf.len() {
            let lookahead = &buf[self.search_idx..];

            debug!("Find boundary loop! Lookahead len: {}", lookahead.len());

            // Look for the boundary, or if it isn't found, stop near the end.
            match twoway::find_bytes(lookahead, &self.boundary) {
                Some(found_idx) => {
                    self.search_idx += found_idx;
                    self.boundary_read = true;
                },
                None => {
                    self.search_idx += lookahead.len().saturating_sub(self.boundary.len() + 2);
                }
            }
        }        
        
        debug!("After-loop Buf len: {} Search idx: {} Boundary read: {:?}",
               buf.len(), self.search_idx, self.boundary_read);

        // If the two bytes before the boundary are a CR-LF, we need to back up
        // the cursor so we don't yield bytes that client code isn't expecting.
        if self.boundary_read && self.search_idx >= 2 {
            let two_bytes_before = &buf[self.search_idx - 2 .. self.search_idx];

            trace!("Two bytes before: {:?} ({:?}) (\"\\r\\n\": {:?})",
                   String::from_utf8_lossy(two_bytes_before), two_bytes_before, b"\r\n");

            if two_bytes_before == &*b"\r\n" {
                debug!("Subtract two!");
                self.search_idx -= 2;
            } 
        }

        let ret_buf = &buf[..self.search_idx];

        trace!("Returning buf: {:?}", String::from_utf8_lossy(ret_buf));

        Ok(ret_buf)
    }

    #[doc(hidden)]
    pub fn consume_boundary(&mut self) -> io::Result<bool> {
        if self.at_end {
            return Ok(true);
        }

        while !(self.boundary_read || self.at_end){
            debug!("Boundary not found yet");

            let buf_len = self.read_to_boundary()?.len();

            debug!("Discarding {} bytes", buf_len);

            if buf_len == 0 {
                break;
            }

            self.consume(buf_len);
        }

        self.source.consume(self.search_idx);
        self.search_idx = 0;

        trace!("Consumed up to self.search_idx, remaining buf: {:?}",
               String::from_utf8_lossy(self.source.get_buf()));

        let consume_amt = {
            let buf = self.source.get_buf();
            let mut skip_size = 0;
            while buf.len() >= skip_size + 2 && buf[skip_size..(skip_size + 2)] == *b"\r\n" {
                skip_size += 2;
            }
            self.boundary.len() + skip_size
        };

        self.source.consume(consume_amt);
        self.boundary_read = false;

        let mut bytes_after = [0, 0];

        let read = self.source.read(&mut bytes_after)?;

        if read == 1 {
            let _ = self.source.read(&mut bytes_after[1..])?;
        }

        if bytes_after == *b"--" {
            self.at_end = true;
        } else if bytes_after != *b"\r\n" {
            debug!("Unexpected bytes following boundary: {:?}", String::from_utf8_lossy(&bytes_after));
        }

        trace!("Consumed boundary (at_end: {:?}), remaining buf: {:?}", self.at_end,
               String::from_utf8_lossy(self.source.get_buf()));

        Ok(self.at_end)
    }
}

#[cfg(feature = "bench")]
impl<'a> BoundaryReader<io::Cursor<&'a [u8]>> {
    fn new_with_bytes(bytes: &'a [u8], boundary: &str) -> Self {
        Self::from_reader(io::Cursor::new(bytes), boundary)
    }

    fn reset(&mut self) {
        // Dump buffer and reset cursor
        self.source.seek(io::SeekFrom::Start(0));
        self.at_end = false;
        self.boundary_read = false;
        self.search_idx = 0;
    }
}

impl<R> Borrow<R> for BoundaryReader<R> {
    fn borrow(&self) -> &R {
        self.source.get_ref()
    }
}

impl<R> Read for BoundaryReader<R> where R: Read {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let read = {
            let mut buf = self.read_to_boundary()?;
            // This shouldn't ever be an error so unwrapping is fine.
            buf.read(out).unwrap()
        };

        self.consume(read);
        Ok(read)
    }
}

impl<R> BufRead for BoundaryReader<R> where R: Read {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.read_to_boundary()
    }

    fn consume(&mut self, amt: usize) {
        let true_amt = cmp::min(amt, self.search_idx);

        debug!("Consume! amt: {} true amt: {}", amt, true_amt);

        self.source.consume(true_amt);
        self.search_idx -= true_amt;
    }
}

fn fill_buf_min<R: Read>(buf: &mut BufReader<R>, min: usize) -> io::Result<&[u8]> {
    const MAX_ATTEMPTS: usize = 3;

    let mut attempts = 0;

    while buf.available() < min && attempts < MAX_ATTEMPTS {
        if buf.read_into_buf()? == 0 { break; };
        attempts += 1;
    }

    Ok(buf.get_buf())
}

#[cfg(test)]
mod test {
    use super::BoundaryReader;

    use std::io;
    use std::io::prelude::*;

    const BOUNDARY: &'static str = "boundary";
    const TEST_VAL: &'static str = "--boundary\r\n\
                                    dashed-value-1\r\n\
                                    --boundary\r\n\
                                    dashed-value-2\r\n\
                                    --boundary--";
        
    #[test]
    fn test_boundary() {
        let _ = ::env_logger::init();        
        debug!("Testing boundary (no split)");

        let src = &mut TEST_VAL.as_bytes();
        let mut reader = BoundaryReader::from_reader(src, BOUNDARY);

        let mut buf = String::new();
        
        test_boundary_reader(&mut reader, &mut buf);
    }

    struct SplitReader<'a> {
        left: &'a [u8],
        right: &'a [u8],
    }

    impl<'a> SplitReader<'a> {
        fn split(data: &'a [u8], at: usize) -> SplitReader<'a> {
            let (left, right) = data.split_at(at);

            SplitReader { 
                left: left,
                right: right,
            }
        }
    }

    impl<'a> Read for SplitReader<'a> {
        fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
            fn copy_bytes_partial(src: &mut &[u8], dst: &mut [u8]) -> usize {
                src.read(dst).unwrap()
            }

            let mut copy_amt = copy_bytes_partial(&mut self.left, dst);

            if copy_amt == 0 {
                copy_amt = copy_bytes_partial(&mut self.right, dst)
            };

            Ok(copy_amt)
        }
    }

    #[test]
    fn test_split_boundary() {
        let _ = ::env_logger::init();        
        debug!("Testing boundary (split)");

        let mut buf = String::new();
        
        // Substitute for `.step_by()` being unstable.
        for split_at in 0 .. TEST_VAL.len(){
            debug!("Testing split at: {}", split_at);

            let src = SplitReader::split(TEST_VAL.as_bytes(), split_at);
            let mut reader = BoundaryReader::from_reader(src, BOUNDARY);
            test_boundary_reader(&mut reader, &mut buf);
        }

    }

    fn test_boundary_reader<R: Read>(reader: &mut BoundaryReader<R>, buf: &mut String) {
        buf.clear();

        debug!("Read 1");
        let _ = reader.read_to_string(buf).unwrap();
        assert!(buf.is_empty(), "Buffer not empty: {:?}", buf);
        buf.clear();

        debug!("Consume 1");
        reader.consume_boundary().unwrap();

        debug!("Read 2");
        let _ = reader.read_to_string(buf).unwrap();
        assert_eq!(buf, "dashed-value-1");
        buf.clear();

        debug!("Consume 2");
        reader.consume_boundary().unwrap();

        debug!("Read 3");
        let _ = reader.read_to_string(buf).unwrap();
        assert_eq!(buf, "dashed-value-2");
        buf.clear();

        debug!("Consume 3");
        reader.consume_boundary().unwrap();

        debug!("Read 4");
        let _ = reader.read_to_string(buf).unwrap();
        assert_eq!(buf, "");
    }

    #[cfg(feature = "bench")]
    mod bench {
        extern crate test;
        use self::test::Bencher;

        use super::*;

        #[bench]
        fn bench_boundary_reader(b: &mut Bencher) {
            let mut reader = BoundaryReader::new_with_bytes(TEST_VAL.as_bytes(), BOUNDARY);
            let mut buf = String::with_capacity(256);

            b.iter(|| {
                reader.reset();
                test_boundary_reader(&mut reader, &mut buf);
            });
        }
    }
}
