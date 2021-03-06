use std::str;
use std::io;
use std::fs::File;
use std::ops::{Range, Deref, Index};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use memmap::Mmap;
use goblin::elf::header as elf_header;
use goblin::elf::section_header::{SHT_SYMTAB, SHT_DYNSYM, SHT_STRTAB};
use goblin::elf::program_header::PT_LOAD;

use elf::{self, Endian};
use utils::{StableIndex, get_major, get_minor};
use archive::{BinaryId, Bitness, Endianness};

enum Blob {
    Mmap( Mmap ),
    StaticSlice( &'static [u8] ),
    Owned( Vec< u8 > )
}

impl Deref for Blob {
    type Target = [u8];

    #[inline]
    fn deref( &self ) -> &Self::Target {
        match *self {
            Blob::Mmap( ref mmap ) => &mmap,
            Blob::StaticSlice( slice ) => slice,
            Blob::Owned( ref bytes ) => &bytes
        }
    }
}

#[derive(Debug)]
pub struct SymbolTable {
    pub range: Range< u64 >,
    pub strtab_range: Range< u64 >,
    pub is_dynamic: bool
}

#[derive(Debug)]
pub struct LoadHeader {
    pub address: u64,
    pub file_offset: u64,
    pub file_size: u64,
    pub memory_size: u64,
    pub alignment: u64,
    pub is_readable: bool,
    pub is_writable: bool,
    pub is_executable: bool
}

pub struct BinaryData {
    id: BinaryId,
    name: String,
    blob: Blob,
    data_range: Option< Range< usize > >,
    text_range: Option< Range< usize > >,
    eh_frame_range: Option< Range< usize > >,
    debug_frame_range: Option< Range< usize > >,
    gnu_debuglink_range: Option< Range< usize > >,
    arm_extab_range: Option< Range< usize > >,
    arm_exidx_range: Option< Range< usize > >,
    is_shared_object: bool,
    symbol_tables: Vec< SymbolTable >,
    load_headers: Vec< LoadHeader >,
    architecture: &'static str,
    endianness: Endianness,
    bitness: Bitness
}

impl BinaryData {
    pub fn load_from_fs< P: AsRef< Path > >( expected_id: Option< BinaryId >, path: P ) -> io::Result< Self > {
        let path = path.as_ref();
        debug!( "Loading binary {:?}...", path );

        let fp = File::open( path )?;
        let mmap = unsafe { Mmap::map( &fp )? };
        let blob = Blob::Mmap( mmap );

        let metadata = fp.metadata()?;
        let inode = metadata.ino();
        let dev = metadata.dev();
        let dev_major = get_major( dev );
        let dev_minor = get_minor( dev );
        let loaded_id = BinaryId { inode, dev_major, dev_minor };

        if let Some( expected_id ) = expected_id {
            if loaded_id != expected_id {
                return Err( io::Error::new( io::ErrorKind::Other, format!( "major/minor/inode of {:?} doesn't match the expected value: {:?} != {:?}", path, loaded_id, expected_id ) ) );
            }
        }

        BinaryData::load( &path.to_string_lossy(), loaded_id, blob )
    }

    pub fn load_from_static_slice( name: &str, id: BinaryId, slice: &'static [u8] ) -> io::Result< Self > {
        debug!( "Loading binary '{}'...", name );

        let blob = Blob::StaticSlice( slice );
        BinaryData::load( name, id, blob )
    }

    pub fn load_from_owned_bytes( name: &str, id: BinaryId, bytes: Vec< u8 > ) -> io::Result< Self > {
        debug!( "Loading binary '{}'...", name );

        let blob = Blob::Owned( bytes );
        BinaryData::load( name, id, blob )
    }

    fn load( path: &str, id: BinaryId, blob: Blob ) -> io::Result< Self > {
        let mut data_range = None;
        let mut text_range = None;
        let mut eh_frame_range = None;
        let mut debug_frame_range = None;
        let mut gnu_debuglink_range = None;
        let mut arm_extab_range = None;
        let mut arm_exidx_range = None;
        let mut is_shared_object = false;
        let mut symbol_tables = Vec::new();
        let mut load_headers = Vec::new();
        let mut endianness = Endianness::LittleEndian;
        let mut bitness = Bitness::B32;
        let mut architecture = "";

        {
            let elf = elf::parse( &blob ).map_err( |err| io::Error::new( io::ErrorKind::Other, err ) )?;
            parse_elf!( elf, |elf| {
                endianness = match elf.endianness() {
                    Endian::Little => Endianness::LittleEndian,
                    Endian::Big => Endianness::BigEndian
                };

                bitness = if elf.is_64_bit() {
                    Bitness::B64
                } else {
                    Bitness::B32
                };

                is_shared_object = match elf.header().e_type {
                    elf_header::ET_EXEC => false,
                    elf_header::ET_DYN => true,
                    _ => {
                        return Err( io::Error::new( io::ErrorKind::Other, format!( "unknown ELF type '{}' for {:?}", elf.header().e_type, path ) ) );
                    }
                };

                architecture = match elf.header().e_machine {
                    elf_header::EM_X86_64 => "amd64",
                    elf_header::EM_386 => "x86",
                    elf_header::EM_ARM => "arm",
                    elf_header::EM_MIPS => {
                        if elf.is_64_bit() {
                            "mips64"
                        } else {
                            "mips"
                        }
                    },
                    kind => {
                        return Err( io::Error::new( io::ErrorKind::Other, format!( "unknown machine type '{}' for {:?}", kind, path ) ) );
                    }
                };

                let name_strtab_header = elf.get_section_header( elf.header().e_shstrndx as usize )
                    .ok_or_else( || io::Error::new( io::ErrorKind::Other, format!( "missing section header for section names strtab for {:?}", path ) ) )?;

                let name_strtab = elf.get_strtab( &name_strtab_header )
                    .ok_or_else( || io::Error::new( io::ErrorKind::Other, format!( "missing strtab for section names strtab for {:?}", path ) ) )?;

                for header in elf.section_headers() {
                    let ty = header.sh_type as u32;
                    if ty == SHT_SYMTAB || ty == SHT_DYNSYM {
                        let is_dynamic = ty == SHT_DYNSYM;
                        let strtab_key = header.sh_link as usize;
                        if let Some( strtab_header ) = elf.get_section_header( strtab_key ) {
                            if strtab_header.sh_type as u32 == SHT_STRTAB {
                                let strtab_range = elf.get_section_body_range( &strtab_header );
                                let symtab_range = elf.get_section_body_range( &header );
                                symbol_tables.push( SymbolTable {
                                    range: symtab_range,
                                    strtab_range,
                                    is_dynamic
                                });
                            }
                        }
                    }

                    let out_range = match name_strtab.get( header.sh_name ) {
                        Some( Ok( ".data" ) ) => &mut data_range,
                        Some( Ok( ".text" ) ) => &mut text_range,
                        Some( Ok( ".eh_frame" ) ) => &mut eh_frame_range,
                        Some( Ok( ".debug_frame" ) ) => &mut debug_frame_range,
                        Some( Ok( ".gnu_debuglink" ) ) => &mut gnu_debuglink_range,
                        Some( Ok( ".ARM.extab" ) ) => &mut arm_extab_range,
                        Some( Ok( ".ARM.exidx" ) ) => &mut arm_exidx_range,
                        _ => continue
                    };

                    let offset = header.sh_offset as usize;
                    let length = header.sh_size as usize;
                    let range = offset..offset + length;
                    if let Some( _ ) = blob.get( range.clone() ) {
                        *out_range = Some( range );
                    }
                }

                for header in elf.program_headers() {
                    if header.p_type != PT_LOAD {
                        continue;
                    }

                    let entry = LoadHeader {
                        address: header.p_vaddr,
                        file_offset: header.p_offset,
                        file_size: header.p_filesz,
                        memory_size: header.p_memsz,
                        alignment: header.p_align,
                        is_readable: header.is_read(),
                        is_writable: header.is_write(),
                        is_executable: header.is_executable()
                    };

                    load_headers.push( entry );
                }

                Ok(())
            })?;
        }

        let binary = BinaryData {
            id,
            name: path.to_string(),
            blob,
            data_range,
            text_range,
            eh_frame_range,
            debug_frame_range,
            gnu_debuglink_range,
            arm_extab_range,
            arm_exidx_range,
            is_shared_object,
            symbol_tables,
            load_headers,
            architecture,
            endianness,
            bitness
        };

        Ok( binary )
    }

    #[inline]
    pub fn id( &self ) -> &BinaryId {
        &self.id
    }

    #[inline]
    pub fn name( &self ) -> &str {
        &self.name
    }

    #[inline]
    pub fn architecture( &self ) -> &str {
        self.architecture
    }

    #[inline]
    pub fn endianness( &self ) -> Endianness {
        self.endianness
    }

    #[inline]
    pub fn bitness( &self ) -> Bitness {
        self.bitness
    }

    #[inline]
    pub fn symbol_tables( &self ) -> &[SymbolTable] {
        &self.symbol_tables
    }

    #[inline]
    pub fn as_bytes( &self ) -> &[u8] {
        &self.blob
    }

    #[inline]
    pub fn is_shared_object( &self ) -> bool {
        self.is_shared_object
    }

    #[inline]
    pub fn data_range( &self ) -> Option< Range< usize > > {
        self.data_range.clone()
    }

    #[inline]
    pub fn text_range( &self ) -> Option< Range< usize > > {
        self.text_range.clone()
    }

    #[inline]
    pub fn eh_frame_range( &self ) -> Option< Range< usize > > {
        self.eh_frame_range.clone()
    }

    #[inline]
    pub fn debug_frame_range( &self ) -> Option< Range< usize > > {
        self.debug_frame_range.clone()
    }

    #[inline]
    pub fn gnu_debuglink_range( &self ) -> Option< Range< usize > > {
        self.gnu_debuglink_range.clone()
    }

    #[inline]
    pub fn arm_extab_range( &self ) -> Option< Range< usize > > {
        self.arm_extab_range.clone()
    }

    #[inline]
    pub fn arm_exidx_range( &self ) -> Option< Range< usize > > {
        self.arm_exidx_range.clone()
    }

    #[inline]
    pub fn load_headers( &self ) -> &[LoadHeader] {
        &self.load_headers
    }
}

impl Deref for BinaryData {
    type Target = [u8];

    #[inline]
    fn deref( &self ) -> &Self::Target {
        self.as_bytes()
    }
}

unsafe impl StableIndex for BinaryData {}

impl Index< Range< u64 > > for BinaryData {
    type Output = [u8];

    #[inline]
    fn index( &self, index: Range< u64 > ) -> &Self::Output {
        &self.as_bytes()[ index.start as usize..index.end as usize ]
    }
}
