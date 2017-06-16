use std::str;
use std::marker::PhantomData;
use std::collections::HashMap;
use err::*;
use object::*;
use types::*;
use xref::{XRef, XRefTable};
use primitive::{Primitive, Stream, Dictionary, PdfString};
use backend::Backend;
use parser::parse;
use parser::parse_object::parse_indirect_object;
use parser::lexer::Lexer;
use parser::parse_xref::read_xref_and_trailer_at;
use stream::{ObjectStream};

pub struct PromisedRef<T> {
    inner:      PlainRef,
    _marker:    PhantomData<T>
}
impl<'a, T> Into<PlainRef> for &'a PromisedRef<T> {
    fn into(self) -> PlainRef {
        self.inner
    }
}
impl<'a, T> Into<Ref<T>> for &'a PromisedRef<T> {
    fn into(self) -> Ref<T> {
        Ref::new(self.into())
    }
}

// tail call
fn find_page<'a>(pages: &'a PageTree, mut offset: i32, page_nr: i32) -> Result<&'a Page> {
    for kid in &pages.kids {
        println!("{}/{} {:?}", offset, page_nr, kid);
        match *kid {
            PagesNode::Tree(ref t) => {
                if offset + t.count < page_nr {
                    offset += t.count;
                } else {
                    return find_page(t, offset, page_nr);
                }
            },
            PagesNode::Leaf(ref p) => {
                if offset < page_nr {
                    offset += 1;
                } else {
                    assert_eq!(offset, page_nr);
                    return Ok(p);
                }
            }
        }
    }
    Err(ErrorKind::PageNotFound {page_nr: page_nr}.into())
}
    
// tail call to trick borrowck
fn update_pages(pages: &mut PageTree, mut offset: i32, page_nr: i32, page: Page) -> Result<()>  {
    for mut kid in &mut pages.kids.iter_mut() {
        println!("{}/{} {:?}", offset, page_nr, kid);
        match *kid {
            PagesNode::Tree(ref mut t) => {
                if offset + t.count < page_nr {
                    offset += t.count;
                } else {
                    return update_pages(t, offset, page_nr, page);
                }
            },
            PagesNode::Leaf(ref mut p) => {
                if offset < page_nr {
                    offset += 1;
                } else {
                    assert_eq!(offset, page_nr);
                    *p = page;
                    return Ok(());
                }
            }
        }
        
    }
    Err(ErrorKind::PageNotFound {page_nr: page_nr}.into())
}

/// Because we need a resolve function to parse the trailer before the File has been created.
fn resolve_helper<B2: Backend>(backend: &B2, refs: &XRefTable, r: PlainRef) -> Result<Primitive> {
    println!("deref({:?})", r); 
    match refs.get(r.id)? {
        XRef::Raw {pos, ..} => {
            let mut lexer = Lexer::new(backend.read(pos..)?);
            Ok(parse_indirect_object(&mut lexer)?.1)
        }
        XRef::Stream {stream_id, index} => {
            let obj_stream = resolve_helper(backend, refs, PlainRef {id: stream_id, gen: 0 /* TODO what gen nr? */})?;
            let obj_stream = ObjectStream::from_primitive(obj_stream, &|r| resolve_helper(backend, refs, r))?;
            let slice = obj_stream.get_object_slice(index)?;
            parse(slice)
        }
        XRef::Free {..} => bail!(ErrorKind::FreeObject {obj_nr: r.id}),
        _ => panic!()
    }
}

pub struct File<B: Backend> {
    backend:    B,
    trailer:    Trailer,
    refs:       XRefTable,
    changes:    HashMap<ObjNr, Primitive>
}

impl<B: Backend> File<B> {
    pub fn new(b: B) -> File<B> {
        File {
            backend: b,
            trailer:    Trailer::default(),
            refs:       XRefTable::new(1), // the root object,
            changes:    HashMap::new()
        }
    }

    pub fn open(path: &str) -> Result<File<B>> {
        let backend = B::open(path)?;
        let xref_offset = locate_xref_offset(backend.read(0..)?)?;

        // TODO: lexer may have to go before xref_offset? Investigate this.
        //      Reason for the doubt: reading previous xref tables/streams
        let (refs, trailer) = {
            let mut lexer = Lexer::new(backend.read(xref_offset..)?);
            
            let (xref_sections, trailer) = read_xref_and_trailer_at(&mut lexer, NO_RESOLVE)?;
            
            let highest_id = trailer.get("Size")
            .ok_or_else(|| ErrorKind::EntryNotFound {key: "Size"})?
            .clone().as_integer()?;

            let mut refs = XRefTable::new(highest_id as ObjNr);
            for section in xref_sections {
                refs.add_entries_from(section);
            }
            
            println!("XRefTable: {:?}", refs);
            println!("Trailer dict: {:?}", trailer);
            let mut prev_trailer = {
                match trailer.get("Prev") {
                    Some(p) => Some(p.as_integer()?),
                    None => None
                }
            };
            while let Some(prev_xref_offset) = prev_trailer {
                println!("adding previous trailer at {}", prev_xref_offset);
                
                let mut lexer = Lexer::new(backend.read(prev_xref_offset as usize..)?);
                let (xref_sections, trailer) = read_xref_and_trailer_at(&mut lexer, NO_RESOLVE)?;
                
                for section in xref_sections {
                    refs.add_entries_from(section);
                }
                
                prev_trailer = {
                    match trailer.get("Prev") {
                        Some(p) => Some(p.as_integer()?),
                        None => None
                    }
                };
            }
            (refs, trailer)
        };
        let trailer = Trailer::from_dict(trailer, &|r| resolve_helper(&backend, &refs, r))?;
        
        Ok(File {
            backend:    backend,
            trailer:    trailer,
            refs:       refs,
            changes:    HashMap::new()
        })
    }

    pub fn get_root(&self) -> &Catalog {
        &self.trailer.root
    }

    fn resolve(&self, r: PlainRef) -> Result<Primitive> {
        match self.changes.get(&r.id) {
            Some(ref p) => Ok((*p).clone()),
            None => resolve_helper(&self.backend, &self.refs, r)
        }
    }

    pub fn deref<T: FromPrimitive>(&self, r: Ref<T>) -> Result<T> {
        let primitive = self.resolve(r.get_inner())?;
        T::from_primitive(primitive, &|id| self.resolve(id))
    }
    pub fn get_num_pages(&self) -> Result<i32> {
        Ok(self.trailer.root.pages.count)
    }
    pub fn get_page(&self, n: i32) -> Result<&Page> {
        if n >= self.get_num_pages()? {
            return Err(ErrorKind::PageOutOfBounds {page_nr: n, max: self.get_num_pages()?}.into());
        }
        find_page(&self.trailer.root.pages, 0, n)
    }
    
    pub fn update_page(&mut self, page_nr: i32, page: Page) -> Result<()> {
        update_pages(&mut self.trailer.root.pages, 0, page_nr, page)
    }
    
    pub fn update(&mut self, id: ObjNr, primitive: Primitive) {
        self.changes.insert(id, primitive);
    }
    
    pub fn promise<T: Object>(&mut self) -> PromisedRef<T> {
        let id = self.refs.len() as u64;
        
        self.refs.push(XRef::Promised);
        
        PromisedRef {
            inner: PlainRef {
                id:     id,
                gen:    0
            },
            _marker:    PhantomData
        }
    }
    
    pub fn fulfill<T>(&mut self, promise: PromisedRef<T>, obj: T) -> Ref<T>
    where T: Into<Primitive>
    {
        self.update(promise.inner.id, obj.into());
        
        Ref::new(promise.inner)
    }
    
    pub fn add<T>(&mut self, obj: T) -> Ref<T> where T: Into<Primitive> {
        let id = self.refs.len() as u64;
        self.refs.push(XRef::Promised);
        self.update(id, obj.into());
        
        Ref::from_id(id)
    }
}

// Returns the value of startxref
fn locate_xref_offset(data: &[u8]) -> Result<usize> {
    // locate the xref offset at the end of the file
    // `\nPOS\n%%EOF` where POS is the position encoded as base 10 integer.
    // u64::MAX has 20 digits + \n\n(2) + %%EOF(5) = 27 bytes max.

    let mut lexer = Lexer::new(data);
    lexer.set_pos_from_end(0);
    lexer.seek_substr_back(b"startxref")?;
    Ok(lexer.next()?.to::<usize>()?)
}

#[derive(Object, FromDict, Default)]
#[pdf(Type=false)]
pub struct Trailer {
    #[pdf(key = "Size")]
    pub highest_id:         i32,

    #[pdf(key = "Prev", opt = true)]
    pub prev_trailer_pos:   Option<i32>,

    #[pdf(key = "Root")]
    pub root:               Catalog,

    #[pdf(key = "Encrypt", opt = true)]
    pub encrypt_dict:       Option<Dictionary>,

    #[pdf(key = "Info", opt = true)]
    pub info_dict:          Option<Dictionary>,

    #[pdf(key = "ID", opt = true)]
    pub id:                 Option<Vec<PdfString>>,
    // TODO ^ Vec<u8> is a String type. Maybe make a wrapper for that
}

#[derive(Object, FromDict, Debug)]
#[pdf(Type = "XRef")]
pub struct XRefInfo {
    // Normal Stream fields
    #[pdf(key = "Filter")]
    filter: Vec<StreamFilter>,

    // XRefStream fields
    #[pdf(key = "Size")]
    pub size: i32,

    #[pdf(key = "Index", opt = true)]
    /// Array of pairs of integers for each subsection, (first object number, number of entries).
    /// Default value (assumed when None): `(0, self.size)`.
    pub index: Option<Vec<i32>>,

    #[pdf(key = "Prev", opt = true)]
    prev: Option<i32>,

    #[pdf(key = "W")]
    pub w: Vec<i32>
}

pub struct XRefStream {
    pub data: Vec<u8>,
    pub info: XRefInfo,
}

impl FromStream for XRefStream {
    fn from_stream(stream: Stream, resolve: &Resolve) -> Result<XRefStream> {
        let info = XRefInfo::from_dict(stream.info, resolve)?;
        println!("XRefInfo: {:?}", info);
        let data = stream.data.to_vec();
        Ok(XRefStream {
            data: data,
            info: info,
        })
    }
}

