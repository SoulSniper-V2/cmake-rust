//! Port of `Source/cmELF.cxx`: a minimal ELF parser used by the
//! `file(RPATH_*)` and `file(READ_ELF)` subcommands.
//!
//! The parser reads the whole file into memory. Reads that C++ performs
//! through stream seeks are reproduced with bounds checks, so out-of-range
//! offsets fail exactly as stream reads past end-of-file do.

use std::collections::HashMap;
use std::fs;

const SHT_DYNAMIC: u64 = 6;

const ET_REL: u16 = 1;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const ET_CORE: u16 = 4;
const ET_LOOS: u16 = 0xfe00;
const ET_HIOS: u16 = 0xfeff;
const ET_LOPROC: u16 = 0xff00;
const ET_HIPROC: u16 = 0xffff;

const EM_MIPS: u16 = 8;

pub const TAG_RPATH: i64 = 15;
pub const TAG_RUNPATH: i64 = 29;
pub const TAG_MIPS_RLD_MAP_REL: i64 = 0x7000_0035;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Invalid,
    RelocatableObject,
    Executable,
    SharedLibrary,
    Core,
    SpecificOS,
    SpecificProc,
}

/// `cmELF::StringEntry`.
#[derive(Clone)]
pub struct StringEntry {
    pub value: String,
    pub position: u64,
    pub size: u64,
    pub index_in_section: i64,
}

/// `cmELF::DynamicEntryList` element: (d_tag, d_val).
pub type DynamicEntry = (i64, u64);

#[derive(Default, Clone)]
struct SectionHeader {
    sh_type: u64,
    sh_link: u64,
    sh_offset: u64,
    sh_size: u64,
    sh_entsize: u64,
}

/// `cmELFInternal` / `cmELFInternalImpl`.
struct Internal {
    data: Vec<u8>,
    // Fields are parsed little-endian when `little` is true.  This starts
    // as the declared byte order; the e_type heuristic may flip it.
    little: bool,
    is64: bool,
    file_type: FileType,
    machine: u16,
    e_shoff: u64,
    e_shentsize: u64,
    e_shnum: u64,
    section_headers: Vec<SectionHeader>,
    dynamic_section_index: i64,
    dynamic_section_entries: Vec<DynamicEntry>,
    dynamic_section_strings: HashMap<u32, StringEntry>,
    error: String,
}

fn file_type_valid(et: u16) -> bool {
    matches!(et, 0 | ET_REL | ET_EXEC | ET_DYN | ET_CORE)
        || (ET_LOOS..=ET_HIOS).contains(&et)
        || (ET_LOPROC..=ET_HIPROC).contains(&et)
}

impl Internal {
    fn new(data: Vec<u8>, little: bool, is64: bool) -> Internal {
        let mut internal = Internal {
            data,
            little,
            is64,
            file_type: FileType::Invalid,
            machine: 0,
            e_shoff: 0,
            e_shentsize: 0,
            e_shnum: 0,
            section_headers: Vec::new(),
            dynamic_section_index: -1,
            dynamic_section_entries: Vec::new(),
            dynamic_section_strings: HashMap::new(),
            error: String::new(),
        };
        internal.read_header();
        internal
    }

    fn u16_at(&self, off: usize) -> Option<u16> {
        let b: [u8; 2] = self.data.get(off..off + 2)?.try_into().ok()?;
        Some(if self.little {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    }

    fn u32_at(&self, off: usize) -> Option<u32> {
        let b: [u8; 4] = self.data.get(off..off + 4)?.try_into().ok()?;
        Some(if self.little {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    }

    fn u64_at(&self, off: usize) -> Option<u64> {
        let b: [u8; 8] = self.data.get(off..off + 8)?.try_into().ok()?;
        Some(if self.little {
            u64::from_le_bytes(b)
        } else {
            u64::from_be_bytes(b)
        })
    }

    /// `cmELFInternalImpl::Read(ELF_Ehdr&)` and the constructor body.
    fn read_header(&mut self) {
        let size = if self.is64 { 64 } else { 52 };
        if self.data.get(..size).is_none() {
            self.set_error("Failed to read main ELF header.");
            return;
        }
        // Byte-order heuristic: if the type is invalid under the declared
        // order but valid when swapped, flip the parse order.
        if let Some(et) = self.u16_at(16) {
            if !file_type_valid(et) {
                let swapped = et.swap_bytes();
                if file_type_valid(swapped) {
                    self.little = !self.little;
                }
            }
        }
        let e_type = self.u16_at(16).unwrap_or(0);
        self.machine = self.u16_at(18).unwrap_or(0);
        self.e_shoff = if self.is64 {
            self.u64_at(40).unwrap_or(0)
        } else {
            self.u32_at(32).unwrap_or(0) as u64
        };
        self.e_shentsize = if self.is64 {
            self.u16_at(58).unwrap_or(0)
        } else {
            self.u16_at(46).unwrap_or(0)
        } as u64;
        self.e_shnum = if self.is64 {
            self.u16_at(60).unwrap_or(0)
        } else {
            self.u16_at(48).unwrap_or(0)
        } as u64;

        self.file_type = match e_type {
            0 => {
                self.set_error("ELF file type is NONE.");
                return;
            }
            ET_REL => FileType::RelocatableObject,
            ET_EXEC => FileType::Executable,
            ET_DYN => FileType::SharedLibrary,
            ET_CORE => FileType::Core,
            et if (ET_LOOS..=ET_HIOS).contains(&et) => FileType::SpecificOS,
            et if (ET_LOPROC..=ET_HIPROC).contains(&et) => FileType::SpecificProc,
            et => {
                self.set_error(&format!("Unknown ELF file type {et}"));
                return;
            }
        };

        // Load the section headers.  The result of loading header 0 is
        // ignored, as upstream does.
        self.section_headers = vec![
            SectionHeader::default();
            if self.e_shnum == 0 { 1 } else { self.e_shnum } as usize
        ];
        self.load_section_header(0);
        self.section_headers
            .resize(self.number_of_sections() as usize, SectionHeader::default());
        for i in 1..self.number_of_sections() as usize {
            if !self.load_section_header(i) {
                self.set_error("Failed to load section headers.");
                return;
            }
        }
    }

    /// `cmELFInternalImpl::Read(ELF_Shdr&)` within `LoadSectionHeader`.
    fn load_section_header(&mut self, i: usize) -> bool {
        let off = self
            .e_shoff
            .wrapping_add(self.e_shentsize.wrapping_mul(i as u64)) as usize;
        let size = if self.is64 { 64 } else { 40 };
        if self.data.get(off..off.wrapping_add(size)).is_none() {
            return false;
        }
        let sh = if self.is64 {
            SectionHeader {
                sh_type: self.u32_at(off + 4).unwrap_or(0) as u64,
                sh_link: self.u32_at(off + 40).unwrap_or(0) as u64,
                sh_offset: self.u64_at(off + 24).unwrap_or(0),
                sh_size: self.u64_at(off + 32).unwrap_or(0),
                sh_entsize: self.u64_at(off + 56).unwrap_or(0),
            }
        } else {
            SectionHeader {
                sh_type: self.u32_at(off + 4).unwrap_or(0) as u64,
                sh_link: self.u32_at(off + 24).unwrap_or(0) as u64,
                sh_offset: self.u32_at(off + 16).unwrap_or(0) as u64,
                sh_size: self.u32_at(off + 20).unwrap_or(0) as u64,
                sh_entsize: self.u32_at(off + 36).unwrap_or(0) as u64,
            }
        };
        if sh.sh_type == SHT_DYNAMIC {
            self.dynamic_section_index = i as i64;
        }
        self.section_headers[i] = sh;
        true
    }

    fn number_of_sections(&self) -> u64 {
        (self.e_shnum + self.section_headers[0].sh_size) as u32 as u64
    }

    /// `cmELFInternalImpl::LoadDynamicSection`.
    fn load_dynamic_section(&mut self) -> bool {
        if self.dynamic_section_index < 0 {
            return false;
        }
        if !self.dynamic_section_entries.is_empty() {
            return true;
        }
        let sec = self.section_headers[self.dynamic_section_index as usize].clone();
        if sec.sh_entsize == 0 {
            return false;
        }
        let n = sec.sh_size / sec.sh_entsize;
        let dyn_size = if self.is64 { 16 } else { 8 };
        for j in 0..n {
            let off = sec.sh_offset.wrapping_add(sec.sh_entsize.wrapping_mul(j)) as usize;
            if self.data.get(off..off.wrapping_add(dyn_size)).is_none() {
                self.set_error("Error reading entry from DYNAMIC section.");
                self.dynamic_section_index = -1;
                return false;
            }
            let entry = if self.is64 {
                (
                    self.u64_at(off).unwrap_or(0) as i64,
                    self.u64_at(off + 8).unwrap_or(0),
                )
            } else {
                (
                    self.u32_at(off).unwrap_or(0) as i32 as i64,
                    self.u32_at(off + 4).unwrap_or(0) as u64,
                )
            };
            self.dynamic_section_entries.push(entry);
        }
        true
    }

    fn get_dynamic_entry_position(&mut self, j: i64) -> u64 {
        if !self.load_dynamic_section() || j < 0 || j >= self.dynamic_section_entries.len() as i64 {
            return 0;
        }
        let sec = &self.section_headers[self.dynamic_section_index as usize];
        sec.sh_offset
            .wrapping_add(sec.sh_entsize.wrapping_mul(j as u64))
    }

    fn get_dynamic_entries(&mut self) -> Vec<DynamicEntry> {
        if !self.load_dynamic_section() {
            return Vec::new();
        }
        self.dynamic_section_entries.clone()
    }

    /// `cmELFInternalImpl::EncodeDynamicEntries`.
    fn encode_dynamic_entries(&self, entries: &[DynamicEntry]) -> Vec<u8> {
        let entry_size = if self.is64 { 16 } else { 8 };
        let mut result = Vec::with_capacity(entry_size * entries.len());
        for &(tag, val) in entries {
            if self.is64 {
                if self.little {
                    result.extend_from_slice(&(tag as u64).to_le_bytes());
                    result.extend_from_slice(&val.to_le_bytes());
                } else {
                    result.extend_from_slice(&(tag as u64).to_be_bytes());
                    result.extend_from_slice(&val.to_be_bytes());
                }
            } else if self.little {
                result.extend_from_slice(&(tag as u32).to_le_bytes());
                result.extend_from_slice(&(val as u32).to_le_bytes());
            } else {
                result.extend_from_slice(&(tag as u32).to_be_bytes());
                result.extend_from_slice(&(val as u32).to_be_bytes());
            }
        }
        result
    }

    /// `cmELFInternalImpl::GetDynamicSectionString`.
    fn get_dynamic_section_string(&mut self, tag: u32) -> Option<StringEntry> {
        if let Some(se) = self.dynamic_section_strings.get(&tag) {
            if se.position > 0 {
                return Some(se.clone());
            }
            return None;
        }
        let mut se = StringEntry {
            value: String::new(),
            position: 0,
            size: 0,
            index_in_section: -1,
        };
        self.dynamic_section_strings.insert(tag, se.clone());
        if !self.load_dynamic_section() {
            return None;
        }
        let sec = self.section_headers[self.dynamic_section_index as usize].clone();
        if sec.sh_link >= self.section_headers.len() as u64 {
            self.set_error("Section DYNAMIC has invalid string table index.");
            return None;
        }
        let strtab = self.section_headers[sec.sh_link as usize].clone();

        let entries = self.dynamic_section_entries.clone();
        for (index, (dtag, dval)) in entries.iter().enumerate() {
            let tagtype = if self.is64 {
                *dtag as u64
            } else {
                *dtag as u32 as u64
            };
            if tagtype != tag as u64 {
                continue;
            }
            if *dval >= strtab.sh_size {
                self.set_error(
                    "Section DYNAMIC references string beyond the end of its string section.",
                );
                return None;
            }
            let first = *dval;
            let mut last = first;
            let end = strtab.sh_size;
            // The stream is seeked to strtab.sh_offset + first; each get()
            // reads one byte, so the byte for `last` is at file offset
            // strtab.sh_offset + last. A failed get() leaves it failed.
            let mut terminated = false;
            let mut failed = false;
            let mut value = Vec::new();
            while last != end {
                let Some(&c) = self.data.get((strtab.sh_offset + last) as usize) else {
                    failed = true;
                    break;
                };
                if terminated && c != 0 {
                    break;
                }
                last += 1;
                if c != 0 {
                    value.push(c);
                } else {
                    terminated = true;
                }
            }
            if failed {
                if tag as i64 == TAG_RPATH {
                    self.set_error("Dynamic section specifies unreadable DT_RPATH");
                } else if tag as i64 == TAG_RUNPATH {
                    self.set_error("Dynamic section specifies unreadable DT_RUNPATH");
                } else if tag as i64 == TAG_MIPS_RLD_MAP_REL {
                    self.set_error("Dynamic section specifies unreadable DT_MIPS_RLD_MAP_REL");
                } else {
                    self.set_error(
                        "Dynamic section specifies unreadable value for unexpected attribute",
                    );
                }
                se.value = String::new();
                self.dynamic_section_strings.insert(tag, se);
                return None;
            }
            // The bytes are kept as-is (non-ASCII paths are UTF-8 on disk).
            se.value = String::from_utf8_lossy(&value).into_owned();
            se.position = strtab.sh_offset + first;
            se.size = last - first;
            se.index_in_section = index as i64;
            self.dynamic_section_strings.insert(tag, se.clone());
            return Some(se);
        }
        None
    }

    fn set_error(&mut self, msg: &str) {
        self.error = msg.to_string();
        self.file_type = FileType::Invalid;
    }
}

/// `cmELF`.
pub struct Elf {
    internal: Option<Internal>,
    error: String,
}

impl Elf {
    pub fn open(path: &str) -> Elf {
        let fail = |error: &str| Elf {
            internal: None,
            error: error.to_string(),
        };
        let Ok(data) = fs::read(path) else {
            return fail("Error opening input file.");
        };
        let Some(ident) = data.get(..16) else {
            return fail("Error reading ELF identification.");
        };
        if !(ident[0] == 0x7f && ident[1] == b'E' && ident[2] == b'L' && ident[3] == b'F') {
            return fail("File does not have a valid ELF identification.");
        }
        let little = match ident[5] {
            1 => true,
            2 => false,
            _ => return fail("ELF file is not LSB or MSB encoded."),
        };
        let is64 = match ident[4] {
            1 => false,
            2 => true,
            _ => return fail("ELF file class is not 32-bit or 64-bit."),
        };
        Elf {
            internal: Some(Internal::new(data, little, is64)),
            error: String::new(),
        }
    }

    pub fn error_message(&self) -> &str {
        match &self.internal {
            Some(i) => &i.error,
            None => &self.error,
        }
    }

    /// `cmELF::Valid` / `operator bool`.
    pub fn is_valid(&self) -> bool {
        self.internal
            .as_ref()
            .is_some_and(|i| i.file_type != FileType::Invalid)
    }

    pub fn has_dynamic_section(&self) -> bool {
        self.internal
            .as_ref()
            .is_some_and(|i| i.file_type != FileType::Invalid && i.dynamic_section_index >= 0)
    }

    pub fn get_dynamic_entry_position(&mut self, index: i64) -> u64 {
        match &mut self.internal {
            Some(i) if i.file_type != FileType::Invalid => i.get_dynamic_entry_position(index),
            _ => 0,
        }
    }

    pub fn get_dynamic_entries(&mut self) -> Vec<DynamicEntry> {
        match &mut self.internal {
            Some(i) if i.file_type != FileType::Invalid => i.get_dynamic_entries(),
            _ => Vec::new(),
        }
    }

    pub fn encode_dynamic_entries(&self, entries: &[DynamicEntry]) -> Vec<u8> {
        match &self.internal {
            Some(i) if i.file_type != FileType::Invalid => i.encode_dynamic_entries(entries),
            _ => Vec::new(),
        }
    }

    /// `cmELF::GetRPath` / `GetRunPath` (restricted to executables and
    /// shared libraries, like upstream).
    pub fn get_rpath(&mut self) -> Option<StringEntry> {
        self.dynamic_string(TAG_RPATH as u32)
    }

    pub fn get_runpath(&mut self) -> Option<StringEntry> {
        self.dynamic_string(TAG_RUNPATH as u32)
    }

    fn dynamic_string(&mut self, tag: u32) -> Option<StringEntry> {
        let i = self.internal.as_mut()?;
        if !matches!(i.file_type, FileType::Executable | FileType::SharedLibrary) {
            return None;
        }
        i.get_dynamic_section_string(tag)
    }

    /// `cmELF::GetMachine` (0 when the file is not a valid ELF).
    pub fn machine(&self) -> u16 {
        self.internal
            .as_ref()
            .filter(|i| i.file_type != FileType::Invalid)
            .map_or(0, |i| i.machine)
    }

    pub fn is_mips(&self) -> bool {
        self.internal
            .as_ref()
            .is_some_and(|i| i.file_type != FileType::Invalid && i.machine == EM_MIPS)
    }
}
