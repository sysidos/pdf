use xref::XRefTable;
use memmap::{Mmap, Protection};
use std::str;
use std::io::Read;
use types::StreamFilter;
use std::io;
use object::*;
use xref::XRef;

pub struct File<B> {
    backend:    B,
    refs:       XRefTable
}

fn locate_xref_offset(data: &[u8]) -> usize {
    // locate the xref offset at the end of the file
    // `\nPOS\n%%EOF` where POS is the position encoded as base 10 integer.
    // u64::MAX has 20 digits + \n\n(2) + %%EOF(5) = 27 bytes max.
    let mut it = data.iter();
    let end = it.rposition(|&n| n == b'\n').unwrap();
    let start = it.rposition(|&n| n == b'\n').unwrap();
    assert_eq!(&data[end ..], b"%%EOF");
    str::from_utf8(&data[start + 1 .. end]).unwrap().parse().unwrap()
}

impl<B> File<B> {
    fn open(path: &str) -> File<Mmap> {
        let file_mmap = Mmap::open_path(path, Protection::Read).unwrap();
        
        let data = file_mmap.as_slice();
        let xref_offset = locate_xref_offset(data);
        println!("xref offset: {}", xref_offset);
        
        unimplemented!()
    }
}

#[test]
fn locate_offset() {
    use std::fs::File;
    let mut buf = Vec::new();
    let mut f = File::open("example.pdf").unwrap();
    f.read_to_end(&mut buf);
    locate_xref_offset(&buf);
}


#[derive(Object, PrimitiveConv)]
#[pdf(Type = "XRef")]
pub struct XRefInfo {
    // Normal Stream fields
    #[pdf(key = "Filter")]
    filter: Vec<StreamFilter>,

    // XRefStream fields
    #[pdf(key = "Size")]
    size: i32,

    #[pdf(key = "Index")]
    index: Vec<i32>,

    #[pdf(key = "Prev")]
    prev: i32,

    #[pdf(key = "W")]
    w: Vec<i32>
}

pub struct XRefStream {
    pub data: Vec<u8>,
    pub info: XRefInfo,
}


#[derive(Object, PrimitiveConv, Default)]
#[pdf(Type = "ObjStm")]
pub struct ObjStmInfo {
    // Normal Stream fields - added as fields are added to Stream
    #[pdf(key = "Filter")]
    pub filter: Vec<StreamFilter>,

    // ObjStmStream fields
    #[pdf(key = "N")]
    pub n: i32,

    #[pdf(key = "First")]
    pub first: i32,

    #[pdf(key = "Extends", opt=true)]
    pub extends: Option<i32>,

}

pub struct ObjectStream<'a, W: io::Write + 'a> {
    pub data:       Vec<u8>,
    /// Byte offset of each object. Index is the object number.
    offsets:    Vec<usize>,
    /// Fields in the stream dictionary.
    pub info:       ObjStmInfo,
    /// The object number of this object.
    id:         ObjNr,

    file:       &'a mut File<W>,
}

impl<'a, W: io::Write + 'a> ObjectStream<'a, W> {
    pub fn new(file: &'a mut File<W>) -> ObjectStream<'a, W> {
        let id = file.promise();
        
        ObjectStream {
            data:       Vec::new(),
            offsets:    Vec::new(),
            info:       ObjStmInfo::default(),
            id:         id,
            file:       file
        }
    }
    pub fn add<T: Object>(&mut self, o: T) -> io::Result<RealizedRef<T>> {
        let start = self.data.len();
        o.serialize(&mut self.data)?;
        let end = self.data.len();
        
        let id = self.file.refs.len() as u64;
        
        self.file.refs.push(XRef::Stream {
            stream_id:  self.id,
            index:      self.items.len()
        });
        
        self.items.push(start);
        
        Ok(RealizedRef {
            id:     id,
            obj:    Box::new(o),
        })
    }
    pub fn fulfill<T: Object>(&mut self, promise: PromisedRef<T>, o: T)
     -> io::Result<RealizedRef<T>>
    {
        let start = self.data.len();
        o.serialize(&mut self.data)?;
        let end = self.data.len();
        
        self.file.refs[promise.id as usize] = XRef::Stream {
            stream_id:  self.id,
            index:      self.items.len() as u32
        };
        
        self.items.push(start);
        
        Ok(RealizedRef {
            id:     promise.id,
            obj:    Box::new(o),
        })
    }
    pub fn finish(self) -> io::Result<PlainRef> {
        let stream_pos = self.file.cursor.position();
        let ref mut out = self.file.cursor;
        
        write!(out, "{} 0 obj\n", self.id)?;
        let indices = self.items.iter().enumerate().map(|(n, item)| format!("{} {}", n, item)).join(" ");
        
        write_dict!(out,
            "/Type"     << "/ObjStm",
            "/Length"   << self.data.len() + indices.len() + 1,
            "/Filter"   << self.filters,
            "/N"        << self.items.len(),
            "/First"    << indices.len() + 1
        );
        write!(out, "\nstream\n{}\n", indices)?;
        out.write(&self.data)?;
        write!(out, "\nendstream\nendobj\n")?;
        
        
        self.file.refs[self.id as usize] = XRef::Raw {
            offset:  stream_pos
        };
        
        Ok(PlainRef {
            id: self.id
        })
    }
}