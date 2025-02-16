//! The Mach 32/64 bit backend for transforming an artifact to a valid, mach-o object file.

use crate::artifact::{
    Data, DataType, Decl, DefinedDecl, Definition, ImportKind, Reloc, SectionKind,
};
use crate::target::make_ctx;
use crate::{Artifact, Ctx};

use failure::Error;
use indexmap::IndexMap;
use scroll::ctx::SizeWith;
use scroll::{IOwrite, Pwrite};
use std::collections::HashMap;
use std::io::SeekFrom::*;
use std::io::{BufWriter, Cursor, Seek, Write};
use string_interner::StringInterner;
use target_lexicon::Architecture;

use goblin::mach::constants::{
    S_ATTR_DEBUG, S_ATTR_PURE_INSTRUCTIONS, S_ATTR_SOME_INSTRUCTIONS, S_CSTRING_LITERALS,
    S_REGULAR, S_ZEROFILL,
};
use goblin::mach::cputype;
use goblin::mach::header::{Header, MH_OBJECT, MH_SUBSECTIONS_VIA_SYMBOLS};
use goblin::mach::load_command::SymtabCommand;
use goblin::mach::relocation::{RelocType, RelocationInfo, SIZEOF_RELOCATION_INFO};
use goblin::mach::segment::{Section, Segment};
use goblin::mach::symbols::Nlist;

struct CpuType(cputype::CpuType);

impl From<Architecture> for CpuType {
    fn from(architecture: Architecture) -> CpuType {
        use goblin::mach::cputype::*;
        use target_lexicon::Architecture::*;
        CpuType(match architecture {
            X86_64 => CPU_TYPE_X86_64,
            I386 | I586 | I686 => CPU_TYPE_X86,
            Aarch64(_) => CPU_TYPE_ARM64,
            Arm(_) => CPU_TYPE_ARM,
            Sparc => CPU_TYPE_SPARC,
            Powerpc => CPU_TYPE_POWERPC,
            Powerpc64 | Powerpc64le => CPU_TYPE_POWERPC64,
            Unknown => 0,
            _ => panic!("requested architecture does not exist in MachO"),
        })
    }
}

fn align_to_align_exp(align: u64) -> u64 {
    assert!(align != 0);
    assert!(align.is_power_of_two());
    let mut align_exp = 0;
    while 1 << align_exp != align {
        align_exp += 1;
    }
    align_exp
}

type SectionIndex = usize;
type StrtableOffset = u64;

const CODE_SECTION_INDEX: SectionIndex = 0;
const DATA_SECTION_INDEX: SectionIndex = 1;
const CSTRING_SECTION_INDEX: SectionIndex = 2;
const BSS_SECTION_INDEX: SectionIndex = 3;
const NUM_DEFAULT_SECTIONS: SectionIndex = 4;

/// A builder for creating a 32/64 bit Mach-o Nlist symbol
#[derive(Debug)]
struct SymbolBuilder {
    name: StrtableOffset,
    section: Option<SectionIndex>,
    global: bool,
    import: bool,
    offset: u64,
    segment_relative_offset: u64,
}

impl SymbolBuilder {
    /// Create a new symbol with `typ`
    pub fn new(name: StrtableOffset) -> Self {
        SymbolBuilder {
            name,
            section: None,
            global: false,
            import: false,
            offset: 0,
            segment_relative_offset: 0,
        }
    }
    /// The section this symbol belongs to
    pub fn section(mut self, section_index: SectionIndex) -> Self {
        self.section = Some(section_index);
        self
    }
    /// Is this symbol global?
    pub fn global(mut self, global: bool) -> Self {
        self.global = global;
        self
    }
    pub fn offset(mut self, offset: u64) -> Self {
        self.offset = offset;
        self
    }
    /// Set the segment relative offset of this symbol, required for relocations
    pub fn relative_offset(mut self, relative_offset: u64) -> Self {
        self.segment_relative_offset = relative_offset;
        self
    }
    /// Returns the offset of this symbol relative to the segment it is apart of
    pub fn get_segment_relative_offset(&self) -> u64 {
        self.segment_relative_offset
    }
    /// Is this symbol an import?
    pub fn import(mut self) -> Self {
        self.import = true;
        self
    }
    /// Finalize and create the symbol
    pub fn create(self) -> Nlist {
        use goblin::mach::symbols::{NO_SECT, N_EXT, N_SECT, N_UNDF};
        let n_strx = self.name;
        let mut n_sect = 0;
        let mut n_type = N_UNDF;
        let mut n_value = self.offset;
        let n_desc = 0;
        if self.global {
            n_type |= N_EXT;
        } else {
            n_type &= !N_EXT;
        }
        if let Some(idx) = self.section {
            n_sect = idx + 1; // add 1 because n_sect expects ordinal
            n_type |= N_SECT;
        }

        if self.import {
            n_sect = NO_SECT as usize;
            // FIXME: this is broken i believe; we need to make it both undefined + global for imports
            n_type = N_EXT;
            n_value = 0;
        } else {
            n_type |= N_SECT;
        }

        Nlist {
            n_strx: n_strx as usize,
            n_type,
            n_sect,
            n_desc,
            n_value,
        }
    }
}

/// An index into the symbol table
type SymbolIndex = usize;

/// Mach relocation builder
#[derive(Debug)]
struct RelocationBuilder {
    symbol: SymbolIndex,
    relocation_offset: u64,
    absolute: bool,
    size: u8,
    r_type: RelocType,
}

impl RelocationBuilder {
    /// Create a relocation for `symbol`, starting at `relocation_offset`
    pub fn new(symbol: SymbolIndex, relocation_offset: u64, r_type: RelocType) -> Self {
        RelocationBuilder {
            symbol,
            relocation_offset,
            absolute: false,
            size: 0,
            r_type,
        }
    }
    /// This is an absolute relocation
    pub fn absolute(mut self) -> Self {
        self.absolute = true;
        self
    }
    /// The size in bytes of the relocated value (defaults to the address size).
    pub fn size(mut self, size: u8) -> Self {
        self.size = size;
        self
    }
    /// Finalize and create the relocation
    pub fn create(self) -> RelocationInfo {
        // it basically goes sort of backwards than what you'd expect because C bitfields are bonkers
        let r_symbolnum: u32 = self.symbol as u32;
        let r_pcrel: u32 = if self.absolute { 0 } else { 1 } << 24;
        let r_length: u32 = match self.size {
            0 => {
                if self.absolute {
                    3
                } else {
                    2
                }
            }
            4 => 2,
            8 => 3,
            size => panic!("unsupported relocation size {}", size),
        } << 25;
        let r_extern: u32 = 1 << 27;
        let r_type = (self.r_type as u32) << 28;
        // r_symbolnum, 24 bits, r_pcrel 1 bit, r_length 2 bits, r_extern 1 bit, r_type 4 bits
        let r_info = r_symbolnum | r_pcrel | r_length | r_extern | r_type;
        RelocationInfo {
            r_address: self.relocation_offset as i32,
            r_info,
        }
    }
}

/// Helper to build sections
#[derive(Debug, Clone)]
struct SectionBuilder {
    addr: u64,
    align: u64,
    offset: u64,
    size: u64,
    flags: u32,
    sectname: String,
    segname: &'static str,
    relocations: Vec<RelocationInfo>,
}

impl SectionBuilder {
    /// Create a new section builder with `sectname`, `segname` and `size`
    pub fn new(sectname: String, segname: &'static str, size: u64) -> Self {
        SectionBuilder {
            addr: 0,
            align: 4,
            offset: 0,
            flags: S_REGULAR,
            size,
            sectname,
            segname,
            relocations: Vec::new(),
        }
    }
    /// Set the vm address of this section
    pub fn addr(mut self, addr: u64) -> Self {
        self.addr = addr;
        self
    }
    /// Set the file offset of this section
    pub fn offset(mut self, offset: u64) -> Self {
        self.offset = offset;
        self
    }
    /// Set the alignment of this section
    pub fn align(mut self, align: u64) -> Self {
        self.align = align;
        self
    }
    /// Set the flags of this section
    pub fn flags(mut self, flags: u32) -> Self {
        self.flags = flags;
        self
    }
    /// Finalize and create the actual Mach-o section
    pub fn create(&self, section_offset: &mut u64, relocation_offset: &mut u64) -> Section {
        let mut sectname = [0u8; 16];
        sectname.pwrite(&*self.sectname, 0).unwrap();
        let mut segname = [0u8; 16];
        segname.pwrite(self.segname, 0).unwrap();
        let mut section = Section {
            sectname,
            segname,
            addr: self.addr,
            size: self.size,
            offset: self.offset as u32,
            align: self.align as u32,
            // FIXME, client needs to set after all offsets known
            reloff: 0,
            nreloc: 0,
            flags: self.flags,
        };
        section.offset = *section_offset as u32;
        *section_offset += section.size;
        if !self.relocations.is_empty() {
            let nrelocs = self.relocations.len();
            section.nreloc = nrelocs as _;
            section.reloff = *relocation_offset as u32;
            *relocation_offset += nrelocs as u64 * SIZEOF_RELOCATION_INFO as u64;
        }
        section
    }
}

type ArtifactCode<'a> = Vec<Definition<'a>>;
type ArtifactData<'a> = Vec<Definition<'a>>;

type StrTableIndex = usize;
type StrTable = StringInterner<StrTableIndex>;
type Symbols = IndexMap<StrTableIndex, SymbolBuilder>;

/// A mach object symbol table
#[derive(Debug)]
struct SymbolTable {
    symbols: Symbols,
    strtable: StrTable,
    indexes: IndexMap<StrTableIndex, SymbolIndex>,
    strtable_size: StrtableOffset,
}

// A manual implementation for Default because StringInterner<usize> does not have a Default impl:
impl Default for SymbolTable {
    fn default() -> Self {
        Self {
            symbols: Symbols::default(),
            strtable: StrTable::new(),
            indexes: IndexMap::default(),
            strtable_size: StrtableOffset::default(),
        }
    }
}

/// The kind of symbol this is
enum SymbolType {
    /// Which `section` this is defined in, the `absolute_offset` in the binary, and its
    /// `segment_relative_offset`
    Defined {
        section: SectionIndex,
        absolute_offset: u64,
        segment_relative_offset: u64,
        global: bool,
    },
    /// An undefined symbol (an import)
    Undefined,
}

impl SymbolTable {
    /// Create a new symbol table. The first strtable entry (like ELF) is always nothing
    pub fn new() -> Self {
        let mut strtable = StrTable::new();
        strtable.get_or_intern("");
        let strtable_size = 1;
        SymbolTable {
            symbols: Symbols::new(),
            strtable,
            strtable_size,
            indexes: IndexMap::new(),
        }
    }
    /// The number of symbols in this table
    pub fn len(&self) -> usize {
        self.symbols.len()
    }
    /// Returns size of the string table, in bytes
    pub fn sizeof_strtable(&self) -> u64 {
        self.strtable_size
    }
    /// Lookup this symbols offset in the segment
    pub fn offset(&self, symbol_name: &str) -> Option<u64> {
        self.strtable
            .get(symbol_name)
            .and_then(|idx| self.symbols.get(&idx))
            .and_then(|sym| Some(sym.get_segment_relative_offset()))
    }
    /// Lookup this symbols ordinal index in the symbol table, if it has one
    pub fn index(&self, symbol_name: &str) -> Option<SymbolIndex> {
        self.strtable
            .get(symbol_name)
            .and_then(|idx| self.indexes.get(&idx).cloned())
    }
    /// Insert a new symbol into this objects symbol table
    pub fn insert(&mut self, symbol_name: &str, kind: SymbolType) {
        // mach-o requires _ prefixes on every symbol, we will allow this to be configurable later
        //let name = format!("_{}", symbol_name);
        let name = symbol_name;
        // 1 for null terminator and 1 for _ prefix (defered until write time);
        let name_len = name.len() as u64 + 1 + 1;
        let last_index = self.strtable.len();
        let name_index = self.strtable.get_or_intern(name);
        debug!("{}: {} <= {}", symbol_name, last_index, name_index);
        // the string is new: NB: relies on name indexes incrementing in sequence, starting at 0
        if name_index == last_index {
            debug!(
                "Inserting new symbol: {}",
                self.strtable.resolve(name_index).unwrap()
            );
            // TODO: add code offset into symbol n_value
            let builder = match kind {
                SymbolType::Undefined => {
                    SymbolBuilder::new(self.strtable_size).global(true).import()
                }
                SymbolType::Defined {
                    section,
                    absolute_offset,
                    global,
                    segment_relative_offset,
                } => SymbolBuilder::new(self.strtable_size)
                    .global(global)
                    .offset(absolute_offset)
                    .relative_offset(segment_relative_offset)
                    .section(section),
            };
            // insert the builder for this symbol, using its strtab index
            self.symbols.insert(name_index, builder);
            // now create the symbols index, and using strtab name as lookup
            self.indexes.insert(name_index, self.symbols.len() - 1);
            // NB do not move this, otherwise all offsets will be off
            self.strtable_size += name_len;
        }
    }
}

#[derive(Debug)]
/// A Mach-o program segment
struct SegmentBuilder {
    /// The sections that belong to this program segment
    pub sections: IndexMap<String, SectionBuilder>,
    /// A stupid offset value I need to refactor out
    pub offset: u64,
    size: u64,
    align_pad_map: HashMap<String, u64>,
}

impl SegmentBuilder {
    /// The size of this segment's _data_, in bytes
    pub fn size(&self) -> u64 {
        self.size
    }
    /// The size of this segment's _load command_, including its associated sections, in bytes
    pub fn load_command_size(&self, ctx: &Ctx) -> u64 {
        Segment::size_with(&ctx) as u64
            + (self.sections.len() as u64 * Section::size_with(&ctx) as u64)
    }
    fn _section_data_file_offset(&self, ctx: &Ctx) -> u64 {
        // section data
        Header::size_with(&ctx.container) as u64 + self.load_command_size(ctx)
    }
    // FIXME: this is in desperate need of refactoring, obviously
    fn build_section(
        symtab: &mut SymbolTable,
        sectname: &'static str,
        segname: &'static str,
        sections: &mut IndexMap<String, SectionBuilder>,
        offset: &mut u64,
        addr: &mut u64,
        symbol_offset: &mut u64,
        section: SectionIndex,
        definitions: &[Definition],
        min_alignment_exponent: u64,
        flags: Option<u32>,
        align_pad_map: &mut HashMap<String, u64>,
    ) {
        let mut local_size = 0;
        let mut section_relative_offset = 0;
        let mut alignment_exponent = min_alignment_exponent;
        let mut def_iter = definitions.iter().peekable();
        while let Some(def) = def_iter.next() {
            if let DefinedDecl::Section { .. } = def.decl {
                unreachable!();
            }

            symtab.insert(
                def.name,
                SymbolType::Defined {
                    section,
                    segment_relative_offset: section_relative_offset,
                    absolute_offset: *symbol_offset,
                    global: def.decl.is_global(),
                },
            );
            *symbol_offset += def.data.file_size() as u64;
            section_relative_offset += def.data.file_size() as u64;
            local_size += def.data.file_size() as u64;

            let next_def_alignment_exponent = std::cmp::max(
                min_alignment_exponent,
                def_iter
                    .peek()
                    .map(|def| align_to_align_exp(def.decl.get_align().unwrap_or(1)))
                    .unwrap_or(0),
            );
            alignment_exponent = std::cmp::max(alignment_exponent, next_def_alignment_exponent);

            let align_pad = (1 << next_def_alignment_exponent)
                - (section_relative_offset % (1 << next_def_alignment_exponent));
            let align_pad = if align_pad == (1 << next_def_alignment_exponent) {
                0
            } else {
                align_pad
            };
            align_pad_map.insert(def.name.to_string(), align_pad);

            *symbol_offset += align_pad;
            section_relative_offset += align_pad;
            local_size += align_pad;
        }
        let mut section = SectionBuilder::new(sectname.to_string(), segname, local_size)
            .offset(*offset)
            .addr(*addr)
            .align(alignment_exponent);
        if let Some(flags) = flags {
            section = section.flags(flags);
        }
        *offset += local_size;
        *addr += local_size;
        sections.insert(sectname.to_string(), section);
    }
    fn build_custom_section(
        symtab: &mut SymbolTable,
        sections: &mut IndexMap<String, SectionBuilder>,
        offset: &mut u64,
        addr: &mut u64,
        symbol_offset: &mut u64,
        section_idx: SectionIndex,
        def: &Definition,
    ) {
        let s = match def.decl {
            DefinedDecl::Section(s) => s,
            _ => unreachable!("in build_custom_section: def.decl != Section"),
        };

        let segment_name = match s.kind() {
            SectionKind::Data => "__DATA",
            SectionKind::Debug => "__DWARF",
            SectionKind::Text => "__TEXT",
        };

        let sectname = if def.name.starts_with(".debug") {
            format!("__debug{}", &def.name[".debug".len()..])
        } else {
            def.name.to_string()
        };

        let mut flags = 0;

        if s.kind() == SectionKind::Debug {
            flags |= S_ATTR_DEBUG;
        }

        for (symbol, symbol_dst_offset) in def.symbols {
            symtab.insert(
                symbol,
                SymbolType::Defined {
                    section: section_idx,
                    segment_relative_offset: *symbol_dst_offset,
                    absolute_offset: *symbol_offset + *symbol_dst_offset,
                    global: true,
                },
            );
        }

        let local_size = def.data.file_size() as u64;
        *symbol_offset += local_size;
        let section = SectionBuilder::new(sectname, segment_name, local_size)
            .offset(*offset)
            .addr(*addr)
            .align(align_to_align_exp(s.get_align().unwrap_or(1)))
            .flags(flags);
        *offset += local_size;
        *addr += local_size;
        sections.insert(def.name.to_string(), section);
    }
    /// Create a new program segment from an `artifact`, symbol table, and context
    // FIXME: this is pub(crate) for now because we can't leak pub(crate) Definition
    pub(crate) fn new(
        artifact: &Artifact,
        code: &[Definition],
        blob_data: &[Definition],
        zeroed_data: &[Definition],
        cstrings: &[Definition],
        custom_sections: &[Definition],
        symtab: &mut SymbolTable,
        ctx: &Ctx,
    ) -> Self {
        let mut offset = Header::size_with(&ctx.container) as u64;
        let mut size = 0;
        let mut symbol_offset = 0;
        let mut sections = IndexMap::new();
        let mut align_pad_map = HashMap::new();

        Self::build_section(
            symtab,
            "__text",
            "__TEXT",
            &mut sections,
            &mut offset,
            &mut size,
            &mut symbol_offset,
            CODE_SECTION_INDEX,
            &code,
            4,
            Some(S_ATTR_PURE_INSTRUCTIONS | S_ATTR_SOME_INSTRUCTIONS),
            &mut align_pad_map,
        );
        Self::build_section(
            symtab,
            "__data",
            "__DATA",
            &mut sections,
            &mut offset,
            &mut size,
            &mut symbol_offset,
            DATA_SECTION_INDEX,
            &blob_data,
            3,
            None,
            &mut align_pad_map,
        );
        Self::build_section(
            symtab,
            "__cstring",
            "__TEXT",
            &mut sections,
            &mut offset,
            &mut size,
            &mut symbol_offset,
            CSTRING_SECTION_INDEX,
            &cstrings,
            0,
            Some(S_CSTRING_LITERALS),
            &mut align_pad_map,
        );
        Self::build_section(
            symtab,
            "__bss",
            "__DATA",
            &mut sections,
            &mut offset,
            &mut size,
            &mut symbol_offset,
            BSS_SECTION_INDEX,
            &zeroed_data,
            0,
            Some(S_ZEROFILL),
            &mut align_pad_map,
        );
        for (idx, def) in custom_sections.iter().enumerate() {
            Self::build_custom_section(
                symtab,
                &mut sections,
                &mut offset,
                &mut size,
                &mut symbol_offset,
                idx + NUM_DEFAULT_SECTIONS,
                def,
            );
        }
        for (ref import, _) in artifact.imports() {
            symtab.insert(import, SymbolType::Undefined);
        }
        // FIXME re add assert
        //assert_eq!(offset, Header::size_with(&ctx.container) + Self::load_command_size(ctx));
        debug!(
            "Segment Size: {} Symtable LoadCommand Offset: {}",
            size, offset
        );
        SegmentBuilder {
            size,
            sections,
            offset,
            align_pad_map,
        }
    }
}

/// A Mach-o object file container
#[derive(Debug)]
struct Mach<'a> {
    ctx: Ctx,
    architecture: Architecture,
    symtab: SymbolTable,
    segment: SegmentBuilder,
    code: ArtifactCode<'a>,
    data: ArtifactData<'a>,
    bss_size: usize,
    cstrings: Vec<Definition<'a>>,
    sections: Vec<Definition<'a>>,
    _p: ::std::marker::PhantomData<&'a ()>,
}

impl<'a> Mach<'a> {
    pub fn new(artifact: &'a Artifact) -> Self {
        let ctx = make_ctx(&artifact.target);
        // FIXME: I believe we can avoid this partition by refactoring SegmentBuilder::new
        let (mut code, mut data, mut bss, mut cstrings, mut sections, mut bss_size) = (
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            0,
        );
        for def in artifact.definitions() {
            match def.decl {
                DefinedDecl::Function { .. } => {
                    code.push(def);
                }
                DefinedDecl::Data(d) => {
                    if let Data::ZeroInit(size) = def.data {
                        bss.push(def);
                        bss_size += size;
                    } else if d.get_datatype() == DataType::String {
                        cstrings.push(def);
                    } else {
                        data.push(def);
                    }
                }
                DefinedDecl::Section(_) => {
                    sections.push(def);
                }
            }
        }

        let mut symtab = SymbolTable::new();
        let mut segment = SegmentBuilder::new(
            &artifact,
            &code,
            &data,
            &bss,
            &cstrings,
            &sections,
            &mut symtab,
            &ctx,
        );
        build_relocations(&mut segment, &artifact, &symtab);

        Mach {
            ctx,
            architecture: artifact.target.architecture,
            symtab,
            segment,
            _p: ::std::marker::PhantomData::default(),
            code,
            data,
            bss_size,
            cstrings,
            sections,
        }
    }
    fn header(&self, sizeofcmds: u64) -> Header {
        let mut header = Header::new(self.ctx);
        header.filetype = MH_OBJECT;
        // safe to divide up the sections into sub-sections via symbols for dead code stripping
        header.flags = MH_SUBSECTIONS_VIA_SYMBOLS;
        header.cputype = CpuType::from(self.architecture).0;
        header.cpusubtype = 3;
        header.ncmds = 2;
        header.sizeofcmds = sizeofcmds as u32;
        header
    }
    pub fn write<T: Write + Seek>(self, file: T) -> Result<(), Error> {
        let mut file = BufWriter::new(file);
        // FIXME: this is ugly af, need cmdsize to get symtable offset
        // construct symtab command
        let mut symtab_load_command = SymtabCommand::new();
        let segment_load_command_size = self.segment.load_command_size(&self.ctx);
        let sizeof_load_commands = segment_load_command_size + symtab_load_command.cmdsize as u64;
        let symtable_offset = self.segment.offset + sizeof_load_commands;
        let strtable_offset =
            symtable_offset + (self.symtab.len() as u64 * Nlist::size_with(&self.ctx) as u64);
        let relocation_offset_start = strtable_offset + self.symtab.sizeof_strtable();
        let first_section_offset = Header::size_with(&self.ctx) as u64 + sizeof_load_commands;
        // start with setting the headers dependent value
        let header = self.header(sizeof_load_commands);

        debug!("Symtable: {:#?}", self.symtab);
        // marshall the sections into something we can actually write
        let mut raw_sections = Cursor::new(Vec::<u8>::new());
        let mut relocation_offset = relocation_offset_start;
        let mut section_offset = first_section_offset;
        for section in self.segment.sections.values() {
            let header = section.create(&mut section_offset, &mut relocation_offset);
            debug!("Section: {:#?}", header);
            raw_sections.iowrite_with(header, self.ctx)?;
        }
        let raw_sections = raw_sections.into_inner();
        debug!(
            "Raw sections len: {} - Section start: {} Strtable size: {} - Segment size: {}",
            raw_sections.len(),
            first_section_offset,
            self.symtab.sizeof_strtable(),
            self.segment.size()
        );

        let mut segment_load_command = Segment::new(self.ctx, &raw_sections);
        segment_load_command.nsects = self.segment.sections.len() as u32;
        // FIXME: de-magic number these
        segment_load_command.initprot = 7;
        segment_load_command.maxprot = 7;
        segment_load_command.filesize = self.segment.size();
        // segment size, with __bss data sizes added
        segment_load_command.vmsize = segment_load_command.filesize + self.bss_size as u64;
        segment_load_command.fileoff = first_section_offset;
        debug!("Segment: {:#?}", segment_load_command);

        debug!("Symtable Offset: {:#?}", symtable_offset);
        assert_eq!(
            symtable_offset,
            self.segment.offset
                + segment_load_command.cmdsize as u64
                + symtab_load_command.cmdsize as u64
        );
        symtab_load_command.nsyms = self.symtab.len() as u32;
        symtab_load_command.symoff = symtable_offset as u32;
        symtab_load_command.stroff = strtable_offset as u32;
        symtab_load_command.strsize = self.symtab.sizeof_strtable() as u32;

        debug!("Symtab Load command: {:#?}", symtab_load_command);

        //////////////////////////////
        // write header
        //////////////////////////////
        file.iowrite_with(header, self.ctx)?;
        debug!("SEEK: after header: {}", file.seek(Current(0))?);

        //////////////////////////////
        // write load commands
        //////////////////////////////
        file.iowrite_with(segment_load_command, self.ctx)?;
        file.write_all(&raw_sections)?;
        file.iowrite_with(symtab_load_command, self.ctx.le)?;
        debug!("SEEK: after load commands: {}", file.seek(Current(0))?);

        //////////////////////////////
        // write code
        //////////////////////////////
        for code in self.code {
            if let Data::Blob(bytes) = code.data {
                file.write_all(&bytes)?;
            } else {
                unreachable!()
            }

            if let Some(&align_pad) = self.segment.align_pad_map.get(code.name) {
                for _ in 0..align_pad {
                    // `0xcc` generates a debug interrupt on x86. When there is no debugger attached
                    // this will abort the program.
                    file.write_all(&[0xcc])?;
                }
            }
        }
        debug!("SEEK: after code: {}", file.seek(Current(0))?);

        //////////////////////////////
        // write data
        //////////////////////////////
        for data in self.data {
            if let Data::Blob(bytes) = data.data {
                file.write_all(bytes)?;
            }

            if let Some(&align_pad) = self.segment.align_pad_map.get(data.name) {
                for _ in 0..align_pad {
                    // Exact padding value doesn't matter. Not using zero to prevent confusion
                    // with a zero pointer when the final executable accidentially reads past
                    // the end of a data object.
                    file.write_all(&[0xaa])?;
                }
            }
        }
        debug!("SEEK: after data: {}", file.seek(Current(0))?);

        //////////////////////////////
        // write cstrings
        //////////////////////////////
        for cstring in self.cstrings {
            if let Data::Blob(bytes) = cstring.data {
                file.write_all(bytes)?;
            } else {
                unreachable!();
            }

            if let Some(&align_pad) = self.segment.align_pad_map.get(cstring.name) {
                for _ in 0..align_pad {
                    // See comment above for explanation of 0xaa
                    file.write_all(&[0xaa])?;
                }
            }
        }
        debug!("SEEK: after cstrings: {}", file.seek(Current(0))?);

        //////////////////////////////
        // write custom sections
        //////////////////////////////
        for section in self.sections {
            if let Data::Blob(bytes) = section.data {
                file.write_all(bytes)?;
            } else {
                unreachable!()
            }

            if let Some(&align_pad) = self.segment.align_pad_map.get(section.name) {
                for _ in 0..align_pad {
                    // See comment above for explanation of 0xaa
                    file.write_all(&[0xaa])?;
                }
            }
        }
        debug!("SEEK: after custom sections: {}", file.seek(Current(0))?);

        //////////////////////////////
        // write symtable
        //////////////////////////////
        for (idx, symbol) in self.symtab.symbols.into_iter() {
            let symbol = symbol.create();
            debug!("{}: {:?}", idx, symbol);
            file.iowrite_with(symbol, self.ctx)?;
        }
        debug!("SEEK: after symtable: {}", file.seek(Current(0))?);

        //////////////////////////////
        // write strtable
        //////////////////////////////
        // we need to write first, empty element - but without an underscore
        file.iowrite(0u8)?;
        for (idx, string) in self.symtab.strtable.into_iter().skip(1) {
            debug!("{}: {:?}", idx, string);
            // yup, an underscore
            file.iowrite(0x5fu8)?;
            file.write_all(string.as_bytes())?;
            file.iowrite(0u8)?;
        }
        debug!("SEEK: after strtable: {}", file.seek(Current(0))?);

        //////////////////////////////
        // write relocations
        //////////////////////////////
        for section in self.segment.sections.values() {
            debug!("Relocations: {}", section.relocations.len());
            for reloc in section.relocations.iter().cloned() {
                debug!("  {:?}", reloc);
                file.iowrite_with(reloc, self.ctx.le)?;
            }
        }
        debug!("SEEK: after relocations: {}", file.seek(Current(0))?);

        file.iowrite(0u8)?;

        Ok(())
    }
}

// FIXME: this should actually return a runtime error if we encounter a from.decl to.decl pair which we don't explicitly match on
fn build_relocations(segment: &mut SegmentBuilder, artifact: &Artifact, symtab: &SymbolTable) {
    use goblin::mach::relocation::{
        R_ABS, X86_64_RELOC_BRANCH, X86_64_RELOC_GOT_LOAD, X86_64_RELOC_SIGNED,
        X86_64_RELOC_UNSIGNED,
    };
    let text_idx = segment.sections.get_full("__text").unwrap().0;
    let data_idx = segment.sections.get_full("__data").unwrap().0;
    debug!("Generating relocations");
    for link in artifact.links() {
        debug!(
            "Import links for: from {} to {} at {:#x} with {:?}",
            link.from.name, link.to.name, link.at, link.reloc
        );
        let (absolute, reloc) = match link.reloc {
            Reloc::Auto => {
                // NB: we currently deduce the meaning of our relocation from from decls -> to decl relocations
                // e.g., global static data references, are constructed from Data -> Data links
                match (link.from.decl, link.to.decl) {
                    // from/to debug section
                    (Decl::Defined(DefinedDecl::Section(s)), _)
                        if s.kind() == SectionKind::Debug =>
                    {
                        panic!("must use Reloc::Debug for debug section links")
                    }
                    // only debug sections should link to debug sections
                    (_, Decl::Defined(DefinedDecl::Section(s)))
                        if s.kind() == SectionKind::Debug =>
                    {
                        panic!("invalid DebugSection link")
                    }

                    // from/to custom section
                    (Decl::Defined(DefinedDecl::Section(_)), _)
                    | (_, Decl::Defined(DefinedDecl::Section(_))) => {
                        panic!("relocations are not yet supported for custom sections")
                    }

                    // from data object
                    (Decl::Defined(DefinedDecl::Data { .. }), _) => (true, X86_64_RELOC_UNSIGNED),

                    // from function
                    (Decl::Defined(DefinedDecl::Function { .. }), to) => match to {
                        Decl::Defined(DefinedDecl::Function { .. }) => (false, X86_64_RELOC_BRANCH),
                        Decl::Import(ImportKind::Function) => (false, X86_64_RELOC_BRANCH),

                        Decl::Defined(DefinedDecl::Data { .. }) => (false, X86_64_RELOC_SIGNED),
                        Decl::Import(ImportKind::Data) => (false, X86_64_RELOC_GOT_LOAD),

                        // handled above
                        Decl::Defined(DefinedDecl::Section { .. }) => unreachable!(),
                    },

                    (Decl::Import(_), _) => {
                        unreachable!("Tried to relocate import???");
                    }
                }
            }
            Reloc::Raw { reloc, addend } => {
                debug_assert!(reloc <= u8::max_value() as u32);
                assert!(addend == 0);
                match reloc as u8 {
                    R_ABS => (true, R_ABS),
                    reloc => (false, reloc),
                }
            }
            Reloc::Debug { size, .. } => {
                if link.to.decl.is_section() {
                    // TODO: not sure if these are needed for Mach
                } else {
                    match symtab.index(link.to.name) {
                        Some(to_symbol_index) => {
                            let builder = RelocationBuilder::new(to_symbol_index, link.at, X86_64_RELOC_UNSIGNED).absolute().size(size);
                            segment.sections[link.from.name].relocations.push(builder.create());
                        }
                        _ => error!("Import Relocation from {} to {} at {:#x} has a missing symbol. Dumping symtab {:?}", link.from.name, link.to.name, link.at, symtab)
                    }
                }
                continue;
            }
        };
        match (symtab.offset(link.from.name), symtab.index(link.to.name)) {
            (Some(base_offset), Some(to_symbol_index)) => {
                debug!("{} offset: {}", link.to.name, base_offset + link.at);
                let builder = RelocationBuilder::new(to_symbol_index, base_offset + link.at, reloc);
                // NB: we currently associate absolute relocations with data relocations; this may prove
                // too fragile for future additions; needs analysis
                if absolute {
                    segment.sections.get_index_mut(data_idx).unwrap().1.relocations.push(builder.absolute().create());
                } else {
                    segment.sections.get_index_mut(text_idx).unwrap().1.relocations.push(builder.create());
                }
            },
            _ => error!("Import Relocation from {} to {} at {:#x} has a missing symbol. Dumping symtab {:?}", link.from.name, link.to.name, link.at, symtab)
        }
    }
}

pub fn to_bytes(artifact: &Artifact) -> Result<Vec<u8>, Error> {
    let mach = Mach::new(&artifact);
    let mut buffer = Cursor::new(Vec::new());
    mach.write(&mut buffer)?;
    Ok(buffer.into_inner())
}
