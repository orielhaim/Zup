use std::collections::BTreeMap;

use super::{MAX_RESOURCE_ID, MAX_RESOURCE_SIZE, PeError, ResourceDocument};

pub const RESOURCE_TYPE_ICON: u16 = 3;
pub const RESOURCE_TYPE_RCDATA: u16 = super::RESOURCE_TYPE_RCDATA;
pub const RESOURCE_TYPE_GROUP_ICON: u16 = 14;

const DIRECTORY_ENTRY_RESOURCE: usize = 2;
const DIRECTORY_ENTRY_SECURITY: usize = 4;

const SECTION_HEADER_SIZE: usize = 40;
const SECTION_NAME_RSRC: &[u8; 8] = b".rsrc\0\0\0";
const SECTION_READABLE_INITIALIZED: u32 = 0x40000040;

const DIR_HEADER_SIZE: usize = 16;
const DIR_ENTRY_SIZE: usize = 8;
const DATA_ENTRY_SIZE: usize = 16;
const SUBDIR_FLAG: u32 = 0x80000000;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Section {
    virtual_address: u32,
    virtual_size: u32,
    raw_size: u32,
    raw_pointer: u32,
}

#[derive(Debug, Clone)]
struct ImageLayout {
    section_alignment: u32,
    file_alignment: u32,
    size_of_image: u32,
    size_of_headers: u32,
    checksum_offset: usize,
    number_of_sections_offset: usize,
    resource_directory_offset: usize,
    optional_end: usize,
    section_table: usize,
    sections: Vec<Section>,
    resource: (u32, u32),
    certificate: (u32, u32),
}

fn read_u16(bytes: &[u8], at: usize) -> Result<u16, PeError> {
    bytes
        .get(at..at + 2)
        .and_then(|field| field.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or(PeError::Invalid)
}

fn read_u32(bytes: &[u8], at: usize) -> Result<u32, PeError> {
    bytes
        .get(at..at + 4)
        .and_then(|field| field.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or(PeError::Invalid)
}

fn layout(bytes: &[u8]) -> Result<ImageLayout, PeError> {
    if bytes.len() < 0x40 || &bytes[..2] != b"MZ" {
        return Err(PeError::Invalid);
    }
    let pe = read_u32(bytes, 0x3c)? as usize;
    if bytes.get(pe..pe + 4) != Some(b"PE\0\0".as_slice()) {
        return Err(PeError::Invalid);
    }
    let coff = pe.checked_add(4).ok_or(PeError::Invalid)?;
    let section_count = read_u16(bytes, coff + 2)? as usize;
    let optional_len = read_u16(bytes, coff + 16)? as usize;
    let optional_offset = coff + 20;
    let optional_end = optional_offset
        .checked_add(optional_len)
        .ok_or(PeError::Invalid)?;
    if bytes.len() < optional_end || section_count == 0 {
        return Err(PeError::Invalid);
    }
    let magic = read_u16(bytes, optional_offset)?;
    let directory_base = match magic {
        0x10b => optional_offset + 96,
        0x20b => optional_offset + 112,
        _ => return Err(PeError::Invalid),
    };
    let directory_count = directory_base
        .checked_sub(optional_offset)
        .and_then(|used| optional_len.checked_sub(used))
        .map(|left| left / 8)
        .ok_or(PeError::Invalid)?;
    let directory = |index: usize| -> Result<(u32, u32), PeError> {
        if index >= directory_count {
            return Ok((0, 0));
        }
        let at = directory_base + index * 8;
        Ok((read_u32(bytes, at)?, read_u32(bytes, at + 4)?))
    };
    let section_table = optional_end;
    let table_end = section_table
        .checked_add(
            section_count
                .checked_mul(SECTION_HEADER_SIZE)
                .ok_or(PeError::Invalid)?,
        )
        .ok_or(PeError::Invalid)?;
    if table_end > bytes.len() {
        return Err(PeError::Invalid);
    }
    let mut sections = Vec::with_capacity(section_count);
    for index in 0..section_count {
        let at = section_table + index * SECTION_HEADER_SIZE;
        sections.push(Section {
            virtual_size: read_u32(bytes, at + 8)?,
            virtual_address: read_u32(bytes, at + 12)?,
            raw_size: read_u32(bytes, at + 16)?,
            raw_pointer: read_u32(bytes, at + 20)?,
        });
    }
    Ok(ImageLayout {
        section_alignment: read_u32(bytes, optional_offset + 32)?,
        file_alignment: read_u32(bytes, optional_offset + 36)?,
        size_of_image: read_u32(bytes, optional_offset + 56)?,
        size_of_headers: read_u32(bytes, optional_offset + 60)?,
        checksum_offset: optional_offset + 64,
        number_of_sections_offset: coff + 2,
        resource_directory_offset: directory_base + DIRECTORY_ENTRY_RESOURCE * 8,
        optional_end,
        section_table,
        sections,
        resource: directory(DIRECTORY_ENTRY_RESOURCE)?,
        certificate: directory(DIRECTORY_ENTRY_SECURITY)?,
    })
}

fn file_offset(layout: &ImageLayout, bytes: &[u8], rva: u32) -> Result<usize, PeError> {
    for section in &layout.sections {
        let mapped = section.virtual_size.max(section.raw_size);
        if rva >= section.virtual_address
            && rva - section.virtual_address < mapped
            && rva - section.virtual_address < section.raw_size
        {
            let offset = section.raw_pointer as usize + (rva - section.virtual_address) as usize;
            if offset < bytes.len() {
                return Ok(offset);
            }
        }
    }
    Err(PeError::Invalid)
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Id(u32),
    Name(Vec<u16>),
}

#[derive(Debug, Clone)]
struct Data {
    lang: u32,
    codepage: u32,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
struct Tree(BTreeMap<Key, BTreeMap<Key, Vec<Data>>>);

fn parse_directory(bytes: &[u8], base: usize, at: usize) -> Result<Vec<(Key, u32, bool)>, PeError> {
    let header = bytes
        .get(at..at + DIR_HEADER_SIZE)
        .ok_or(PeError::Invalid)?;
    let named = u16::from_le_bytes(header[12..14].try_into().unwrap()) as usize;
    let ids = u16::from_le_bytes(header[14..16].try_into().unwrap()) as usize;
    let count = named.checked_add(ids).ok_or(PeError::Invalid)?;
    if at
        .checked_add(DIR_HEADER_SIZE + count.checked_mul(DIR_ENTRY_SIZE).ok_or(PeError::Invalid)?)
        .ok_or(PeError::Invalid)?
        > bytes.len()
    {
        return Err(PeError::Invalid);
    }
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let at = at + DIR_HEADER_SIZE + index * DIR_ENTRY_SIZE;
        let name_or_id = read_u32(bytes, at)?;
        let offset = read_u32(bytes, at + 4)?;
        let key = if name_or_id & SUBDIR_FLAG != 0 {
            let string_at = base
                .checked_add((name_or_id & !SUBDIR_FLAG) as usize)
                .ok_or(PeError::Invalid)?;
            let length = read_u16(bytes, string_at)? as usize;
            if string_at
                .checked_add(2 + length.checked_mul(2).ok_or(PeError::Invalid)?)
                .ok_or(PeError::Invalid)?
                > bytes.len()
            {
                return Err(PeError::Invalid);
            }
            let mut name = Vec::with_capacity(length);
            for unit in 0..length {
                name.push(read_u16(bytes, string_at + 2 + unit * 2)?);
            }
            Key::Name(name)
        } else {
            Key::Id(name_or_id)
        };
        out.push((key, offset & !SUBDIR_FLAG, offset & SUBDIR_FLAG != 0));
    }
    Ok(out)
}

fn read_tree(bytes: &[u8], layout: &ImageLayout) -> Result<Tree, PeError> {
    let mut tree = Tree::default();
    let (rva, size) = layout.resource;
    if rva == 0 || size == 0 {
        return Ok(tree);
    }
    let base = file_offset(layout, bytes, rva)?;
    if base.checked_add(size as usize).ok_or(PeError::Invalid)? > bytes.len() {
        return Err(PeError::Invalid);
    }
    for (type_key, type_offset, is_dir) in parse_directory(bytes, base, base)? {
        if !is_dir {
            return Err(PeError::Invalid);
        }
        for (name_key, name_offset, is_dir) in
            parse_directory(bytes, base, base + type_offset as usize)?
        {
            if !is_dir {
                return Err(PeError::Invalid);
            }
            for (lang_key, data_offset, is_dir) in
                parse_directory(bytes, base, base + name_offset as usize)?
            {
                if is_dir {
                    return Err(PeError::Invalid);
                }
                let lang = match lang_key {
                    Key::Id(lang) => lang,
                    Key::Name(_) => return Err(PeError::Invalid),
                };
                let at = base
                    .checked_add(data_offset as usize)
                    .ok_or(PeError::Invalid)?;
                let entry = bytes
                    .get(at..at + DATA_ENTRY_SIZE)
                    .ok_or(PeError::Invalid)?;
                let data_rva = u32::from_le_bytes(entry[..4].try_into().unwrap());
                let size = u32::from_le_bytes(entry[4..8].try_into().unwrap()) as usize;
                let codepage = u32::from_le_bytes(entry[8..12].try_into().unwrap());
                let start = file_offset(layout, bytes, data_rva)?;
                let data = bytes
                    .get(start..start + size)
                    .ok_or(PeError::Invalid)?
                    .to_vec();
                tree.0
                    .entry(type_key.clone())
                    .or_default()
                    .entry(name_key.clone())
                    .or_default()
                    .push(Data {
                        lang,
                        codepage,
                        bytes: data,
                    });
            }
        }
    }
    Ok(tree)
}

pub fn read_resource(image: &[u8], type_id: u16, id: u32) -> Result<Option<Vec<u8>>, PeError> {
    let layout = layout(image)?;
    let tree = read_tree(image, &layout)?;
    let Some(names) = tree.0.get(&Key::Id(u32::from(type_id))) else {
        return Ok(None);
    };
    let Some(variants) = names.get(&Key::Id(id)) else {
        return Ok(None);
    };
    let data = variants
        .iter()
        .find(|data| data.lang == 0)
        .or_else(|| variants.first());
    Ok(data.map(|data| data.bytes.clone()))
}

fn align_up(value: u64, alignment: u64) -> Result<u64, PeError> {
    if alignment == 0 {
        return Err(PeError::Invalid);
    }
    let over = value % alignment;
    if over == 0 {
        return Ok(value);
    }
    value.checked_add(alignment - over).ok_or(PeError::Invalid)
}

fn sort_key(key: &Key) -> (u8, Vec<u16>, u32) {
    match key {
        Key::Name(name) => (0, name.clone(), 0),
        Key::Id(id) => (1, Vec::new(), *id),
    }
}

fn write_header(out: &mut [u8], at: usize, named: u16, ids: u16) {
    out[at..at + 12].fill(0);
    out[at + 12..at + 14].copy_from_slice(&named.to_le_bytes());
    out[at + 14..at + 16].copy_from_slice(&ids.to_le_bytes());
}

fn serialize_tree(tree: &Tree, section_rva: u32) -> Result<Vec<u8>, PeError> {
    type Named<'a> = Vec<(&'a Key, Vec<&'a Data>)>;
    type Typed<'a> = Vec<(&'a Key, Named<'a>)>;
    let mut types: Typed<'_> = Vec::with_capacity(tree.0.len());
    for (type_key, names) in &tree.0 {
        let mut ordered: Named<'_> = Vec::with_capacity(names.len());
        for (name_key, variants) in names {
            if variants.len() > u16::MAX as usize {
                return Err(PeError::TooManyResources {
                    count: variants.len(),
                    limit: MAX_RESOURCE_ID,
                });
            }
            let mut langs: Vec<&Data> = variants.iter().collect();
            langs.sort_by_key(|data| data.lang);
            ordered.push((name_key, langs));
        }
        ordered.sort_by_key(|item| sort_key(item.0));
        types.push((type_key, ordered));
    }
    types.sort_by_key(|item| sort_key(item.0));
    if types.len() > u16::MAX as usize {
        return Err(PeError::TooManyResources {
            count: types.len(),
            limit: MAX_RESOURCE_ID,
        });
    }

    let payload_count: usize = types
        .iter()
        .flat_map(|(_, names)| names)
        .map(|(_, langs)| langs.len())
        .sum();
    let mut strings: BTreeMap<Vec<u16>, usize> = BTreeMap::new();
    let mut strings_size = 0usize;
    for (type_key, names) in &types {
        for key in std::iter::once(*type_key).chain(names.iter().map(|(name, _)| *name)) {
            if let Key::Name(name) = key
                && !strings.contains_key(name)
            {
                strings.insert(name.clone(), strings_size);
                strings_size += 2 + name.len() * 2;
            }
        }
    }

    let mut cursor = DIR_HEADER_SIZE + types.len() * DIR_ENTRY_SIZE;
    let mut type_at: Vec<usize> = Vec::with_capacity(types.len());
    let mut name_at: Vec<Vec<usize>> = Vec::with_capacity(types.len());
    for (_, names) in &types {
        if names.len() > u16::MAX as usize {
            return Err(PeError::TooManyResources {
                count: names.len(),
                limit: MAX_RESOURCE_ID,
            });
        }
        type_at.push(cursor);
        cursor += DIR_HEADER_SIZE + names.len() * DIR_ENTRY_SIZE;
        let mut row = Vec::with_capacity(names.len());
        for (_, langs) in names {
            row.push(cursor);
            cursor += DIR_HEADER_SIZE + langs.len() * DIR_ENTRY_SIZE;
        }
        name_at.push(row);
    }
    let data_entries_at = cursor;
    cursor += payload_count * DATA_ENTRY_SIZE;
    let strings_at = cursor;
    cursor += strings_size;

    let mut payload_offsets: Vec<usize> = Vec::with_capacity(payload_count);
    let mut payload_base = cursor;
    for (_, names) in &types {
        for (_, langs) in names {
            for data in langs {
                payload_base += (4 - (payload_base % 4)) % 4;
                if payload_base
                    .checked_add(data.bytes.len())
                    .ok_or(PeError::Invalid)?
                    > u32::MAX as usize
                    || u64::from(section_rva)
                        .checked_add(payload_base as u64)
                        .ok_or(PeError::Invalid)?
                        > u64::from(u32::MAX)
                {
                    return Err(PeError::ResourceTooLarge {
                        size: data.bytes.len() as u64,
                        limit: MAX_RESOURCE_SIZE,
                    });
                }
                payload_offsets.push(payload_base);
                payload_base += data.bytes.len();
            }
        }
    }

    let mut out = vec![0u8; payload_base];
    let encode = |key: &Key| -> u32 {
        match key {
            Key::Id(id) => *id,
            Key::Name(name) => SUBDIR_FLAG | (strings_at + strings[name]) as u32,
        }
    };

    write_header(&mut out, 0, 0, types.len() as u16);
    let mut type_entry_at = DIR_HEADER_SIZE;
    for (index, (type_key, _)) in types.iter().enumerate() {
        out[type_entry_at..type_entry_at + 4].copy_from_slice(&encode(type_key).to_le_bytes());
        out[type_entry_at + 4..type_entry_at + 8]
            .copy_from_slice(&(type_at[index] as u32 | SUBDIR_FLAG).to_le_bytes());
        type_entry_at += DIR_ENTRY_SIZE;
    }

    let mut payload_index = 0usize;
    let mut data_entry_at = data_entries_at;
    for (type_position, (_, names)) in types.iter().enumerate() {
        let dir_at = type_at[type_position];
        write_header(&mut out, dir_at, 0, names.len() as u16);
        let name_dirs = &name_at[type_position];
        let mut name_entry_at = dir_at + DIR_HEADER_SIZE;
        for (index, (name_key, _)) in names.iter().enumerate() {
            out[name_entry_at..name_entry_at + 4].copy_from_slice(&encode(name_key).to_le_bytes());
            out[name_entry_at + 4..name_entry_at + 8]
                .copy_from_slice(&(name_dirs[index] as u32 | SUBDIR_FLAG).to_le_bytes());
            name_entry_at += DIR_ENTRY_SIZE;
        }
        for (index, (_, langs)) in names.iter().enumerate() {
            let at = name_dirs[index];
            write_header(&mut out, at, 0, langs.len() as u16);
            let mut lang_entry_at = at + DIR_HEADER_SIZE;
            for data in langs {
                let data_rva = section_rva + payload_offsets[payload_index] as u32;
                out[data_entry_at..data_entry_at + 4].copy_from_slice(&data_rva.to_le_bytes());
                out[data_entry_at + 4..data_entry_at + 8]
                    .copy_from_slice(&(data.bytes.len() as u32).to_le_bytes());
                out[data_entry_at + 8..data_entry_at + 12]
                    .copy_from_slice(&data.codepage.to_le_bytes());
                out[lang_entry_at..lang_entry_at + 4].copy_from_slice(&data.lang.to_le_bytes());
                out[lang_entry_at + 4..lang_entry_at + 8]
                    .copy_from_slice(&(data_entry_at as u32).to_le_bytes());
                lang_entry_at += DIR_ENTRY_SIZE;
                data_entry_at += DATA_ENTRY_SIZE;
                let start = payload_offsets[payload_index];
                out[start..start + data.bytes.len()].copy_from_slice(&data.bytes);
                payload_index += 1;
            }
        }
    }

    let mut emitted: BTreeMap<Vec<u16>, ()> = BTreeMap::new();
    let mut strings_cursor = strings_at;
    for (type_key, names) in &types {
        for key in std::iter::once(*type_key).chain(names.iter().map(|(name, _)| *name)) {
            if let Key::Name(name) = key
                && !emitted.contains_key(name)
            {
                emitted.insert(name.clone(), ());
                out[strings_cursor..strings_cursor + 2]
                    .copy_from_slice(&(name.len() as u16).to_le_bytes());
                strings_cursor += 2;
                for unit in name {
                    out[strings_cursor..strings_cursor + 2].copy_from_slice(&unit.to_le_bytes());
                    strings_cursor += 2;
                }
            }
        }
    }

    Ok(out)
}

fn merge(tree: &mut Tree, type_id: u16, documents: &[ResourceDocument]) -> Result<(), PeError> {
    for document in documents {
        if document.id > MAX_RESOURCE_ID || document.bytes.len() as u64 > MAX_RESOURCE_SIZE {
            return Err(PeError::TooManyResources {
                count: documents.len(),
                limit: MAX_RESOURCE_ID,
            });
        }
        if document.id > u32::MAX as usize {
            return Err(PeError::TooManyResources {
                count: documents.len(),
                limit: MAX_RESOURCE_ID,
            });
        }
    }
    let names = tree.0.entry(Key::Id(u32::from(type_id))).or_default();
    for document in documents {
        names.insert(
            Key::Id(document.id as u32),
            vec![Data {
                lang: 0,
                codepage: 0,
                bytes: document.bytes.clone(),
            }],
        );
    }
    Ok(())
}

pub fn update_rcdata(template: &[u8], documents: &[ResourceDocument]) -> Result<Vec<u8>, PeError> {
    super::check_documents(documents, super::RESOURCE_ID_INDEX)?;
    let layout = layout(template)?;
    if layout.certificate != (0, 0) {
        return Err(PeError::Invalid);
    }
    let mut tree = read_tree(template, &layout)?;
    merge(&mut tree, RESOURCE_TYPE_RCDATA, documents)?;
    write_tree(template, &tree)
}

pub fn update_icons(image: &[u8], images: &[Vec<u8>], group: &[u8]) -> Result<Vec<u8>, PeError> {
    if images.is_empty() || group.is_empty() {
        return Err(PeError::Invalid);
    }
    if images.len() > MAX_RESOURCE_ID {
        return Err(PeError::TooManyResources {
            count: images.len(),
            limit: MAX_RESOURCE_ID,
        });
    }
    let layout = layout(image)?;
    if layout.certificate != (0, 0) {
        return Err(PeError::Invalid);
    }
    let mut tree = read_tree(image, &layout)?;
    let icons: Vec<ResourceDocument> = images
        .iter()
        .enumerate()
        .map(|(index, bytes)| ResourceDocument {
            id: index + 1,
            bytes: bytes.clone(),
        })
        .collect();
    merge(&mut tree, RESOURCE_TYPE_ICON, &icons)?;
    merge(
        &mut tree,
        RESOURCE_TYPE_GROUP_ICON,
        &[ResourceDocument {
            id: 1,
            bytes: group.to_vec(),
        }],
    )?;
    write_tree(image, &tree)
}

/// Replace the image's resource section with the serialized tree.
///
/// A previous tree's section header is removed, and its file bytes reclaimed
/// when they sit at the end of the file, so repeated updates keep exactly one
/// live resource section. Appending instead would orphan a full copy of the
/// payload per write, which the loader never reads but every installer ships.
fn write_tree(template: &[u8], tree: &Tree) -> Result<Vec<u8>, PeError> {
    let mut bytes = template.to_vec();
    remove_resource_section(&mut bytes)?;
    let layout = layout(&bytes)?;
    append_section(&bytes, &layout, tree)
}

/// Drop the section the resource directory points at, if any.
///
/// Overlapping or unmappable layouts are refused rather than truncated: this
/// runs before any write the caller cannot take back.
fn remove_resource_section(bytes: &mut Vec<u8>) -> Result<(), PeError> {
    let layout = layout(bytes)?;
    let (rva, _) = layout.resource;
    if rva == 0 {
        return Ok(());
    }
    let position = layout
        .sections
        .iter()
        .position(|section| {
            let extent = section.virtual_size.max(section.raw_size);
            rva >= section.virtual_address && rva - section.virtual_address < extent
        })
        .ok_or(PeError::Invalid)?;
    let raw_start = u64::from(layout.sections[position].raw_pointer);
    let raw_size = u64::from(layout.sections[position].raw_size);
    let raw_end = raw_start.checked_add(raw_size).ok_or(PeError::Invalid)?;
    for (index, other) in layout.sections.iter().enumerate() {
        if index == position || other.raw_size == 0 {
            continue;
        }
        let other_start = u64::from(other.raw_pointer);
        let other_end = other_start
            .checked_add(u64::from(other.raw_size))
            .ok_or(PeError::Invalid)?;
        if other_start < raw_end && raw_start < other_end {
            return Err(PeError::Invalid);
        }
    }
    let count = layout.sections.len();
    let table = layout.section_table;
    bytes.copy_within(
        table + (position + 1) * SECTION_HEADER_SIZE..table + count * SECTION_HEADER_SIZE,
        table + position * SECTION_HEADER_SIZE,
    );
    let vacated = table + (count - 1) * SECTION_HEADER_SIZE;
    bytes[vacated..vacated + SECTION_HEADER_SIZE].fill(0);
    let narrowed = u16::try_from(count - 1).map_err(|_| PeError::Invalid)?;
    let at = layout.number_of_sections_offset;
    bytes[at..at + 2].copy_from_slice(&narrowed.to_le_bytes());
    let directory = layout.resource_directory_offset;
    bytes[directory..directory + 8].fill(0);
    // Recompute SizeOfImage from the sections that remain. Leaving the old
    // value in place would push the replacement section one page past the
    // file offset it reuses on every update, growing the virtual image while
    // the file stays put; images rewritten that way fail to start (os error
    // 193). The replacement append recomputes it again from its own section.
    let alignment = u64::from(layout.section_alignment);
    let mut highest = u64::from(layout.size_of_headers);
    for (index, section) in layout.sections.iter().enumerate() {
        if index == position {
            continue;
        }
        let end = u64::from(section.virtual_address)
            .checked_add(u64::from(section.virtual_size))
            .ok_or(PeError::Invalid)?;
        highest = highest.max(end);
    }
    let shrunk = align_up(highest.max(1), alignment)? as u32;
    let size_at = layout.checksum_offset - 8;
    bytes[size_at..size_at + 4].copy_from_slice(&shrunk.to_le_bytes());
    bytes[layout.checksum_offset..layout.checksum_offset + 4].fill(0);
    if raw_end == bytes.len() as u64 {
        bytes.truncate(raw_start as usize);
    }
    Ok(())
}

fn append_section(template: &[u8], layout: &ImageLayout, tree: &Tree) -> Result<Vec<u8>, PeError> {
    if layout.resource_directory_offset + 8 > layout.optional_end {
        return Err(PeError::Invalid);
    }
    let section_count = layout.sections.len();
    if section_count >= u16::MAX as usize {
        return Err(PeError::HeaderFull);
    }
    let new_header_at = layout
        .section_table
        .checked_add(
            section_count
                .checked_mul(SECTION_HEADER_SIZE)
                .ok_or(PeError::Invalid)?,
        )
        .ok_or(PeError::Invalid)?;
    if new_header_at
        .checked_add(SECTION_HEADER_SIZE)
        .ok_or(PeError::Invalid)?
        > layout.size_of_headers as usize
    {
        return Err(PeError::HeaderFull);
    }
    let section_alignment = u64::from(layout.section_alignment);
    let file_alignment = u64::from(layout.file_alignment);
    if section_alignment == 0 || file_alignment == 0 {
        return Err(PeError::Invalid);
    }
    let section_rva = align_up(u64::from(layout.size_of_image), section_alignment)? as u32;
    if layout
        .sections
        .iter()
        .any(|section| section.virtual_address == section_rva)
    {
        return Err(PeError::Invalid);
    }
    let serialized = serialize_tree(tree, section_rva)?;
    let raw_pointer = align_up(template.len() as u64, file_alignment)?;
    let raw_size = align_up(serialized.len() as u64, file_alignment)?;
    let size_of_image = align_up(
        u64::from(section_rva) + serialized.len() as u64,
        section_alignment,
    )? as u32;

    let mut out = template.to_vec();
    out.resize(raw_pointer as usize, 0);
    out.extend_from_slice(&serialized);
    out.resize(raw_pointer as usize + raw_size as usize, 0);

    let patch_u16 = |out: &mut [u8], at: usize, value: u16| {
        out[at..at + 2].copy_from_slice(&value.to_le_bytes());
    };
    let patch_u32 = |out: &mut [u8], at: usize, value: u32| {
        out[at..at + 4].copy_from_slice(&value.to_le_bytes());
    };
    patch_u16(
        &mut out,
        layout.number_of_sections_offset,
        (section_count + 1) as u16,
    );
    patch_u32(&mut out, layout.resource_directory_offset, section_rva);
    patch_u32(
        &mut out,
        layout.resource_directory_offset + 4,
        serialized.len() as u32,
    );
    patch_u32(&mut out, layout.checksum_offset - 8, size_of_image);
    out[layout.checksum_offset..layout.checksum_offset + 4].fill(0);

    let mut header = [0u8; SECTION_HEADER_SIZE];
    header[..8].copy_from_slice(SECTION_NAME_RSRC);
    header[8..12].copy_from_slice(&(serialized.len() as u32).to_le_bytes());
    header[12..16].copy_from_slice(&section_rva.to_le_bytes());
    header[16..20].copy_from_slice(&(raw_size as u32).to_le_bytes());
    header[20..24].copy_from_slice(&(raw_pointer as u32).to_le_bytes());
    header[36..40].copy_from_slice(&SECTION_READABLE_INITIALIZED.to_le_bytes());
    out[new_header_at..new_header_at + SECTION_HEADER_SIZE].copy_from_slice(&header);

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_image() -> Vec<u8> {
        let mut bytes = vec![0u8; 0x600];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        bytes[0x40..0x44].copy_from_slice(b"PE\0\0");
        bytes[0x44..0x46].copy_from_slice(&0x8664u16.to_le_bytes());
        bytes[0x46..0x48].copy_from_slice(&1u16.to_le_bytes());
        bytes[0x54..0x56].copy_from_slice(&240u16.to_le_bytes());
        bytes[0x58..0x5a].copy_from_slice(&0x20bu16.to_le_bytes());
        bytes[0x58 + 32..0x58 + 36].copy_from_slice(&0x1000u32.to_le_bytes());
        bytes[0x58 + 36..0x58 + 40].copy_from_slice(&0x200u32.to_le_bytes());
        bytes[0x58 + 56..0x58 + 60].copy_from_slice(&0x3000u32.to_le_bytes());
        bytes[0x58 + 60..0x58 + 64].copy_from_slice(&0x400u32.to_le_bytes());
        bytes[0x58 + 108..0x58 + 112].copy_from_slice(&16u32.to_le_bytes());
        let table = 0x58 + 240;
        bytes[table..table + 8].copy_from_slice(b".text\0\0\0");
        bytes[table + 8..table + 12].copy_from_slice(&0x1000u32.to_le_bytes());
        bytes[table + 12..table + 16].copy_from_slice(&0x1000u32.to_le_bytes());
        bytes[table + 16..table + 20].copy_from_slice(&0x200u32.to_le_bytes());
        bytes[table + 20..table + 24].copy_from_slice(&0x400u32.to_le_bytes());
        bytes
    }

    fn document(id: usize, text: &str) -> ResourceDocument {
        ResourceDocument {
            id,
            bytes: text.as_bytes().to_vec(),
        }
    }

    #[test]
    fn written_documents_read_back() {
        let image = minimal_image();
        assert_eq!(read_resource(&image, 10, 1).unwrap(), None);
        let updated = update_rcdata(
            &image,
            &[
                document(1, "index"),
                document(2, "blob"),
                document(65534, "preset"),
            ],
        )
        .unwrap();
        assert_eq!(
            read_resource(&updated, 10, 1).unwrap(),
            Some(b"index".to_vec())
        );
        assert_eq!(
            read_resource(&updated, 10, 2).unwrap(),
            Some(b"blob".to_vec())
        );
        assert_eq!(
            read_resource(&updated, 10, 65534).unwrap(),
            Some(b"preset".to_vec())
        );
        assert_eq!(read_resource(&updated, 10, 3).unwrap(), None);
        assert_eq!(read_resource(&updated, 11, 1).unwrap(), None);
    }

    #[test]
    fn icons_merge_with_existing_documents() {
        let image = minimal_image();
        let composed = update_rcdata(&image, &[document(1, "index"), document(5, "blob")]).unwrap();
        let stamped = update_icons(&composed, &[b"icon".to_vec()], b"group").unwrap();
        assert_eq!(
            read_resource(&stamped, 10, 1).unwrap(),
            Some(b"index".to_vec())
        );
        assert_eq!(
            read_resource(&stamped, 10, 5).unwrap(),
            Some(b"blob".to_vec())
        );
        assert_eq!(
            read_resource(&stamped, RESOURCE_TYPE_ICON, 1).unwrap(),
            Some(b"icon".to_vec())
        );
        assert_eq!(
            read_resource(&stamped, RESOURCE_TYPE_GROUP_ICON, 1).unwrap(),
            Some(b"group".to_vec())
        );
    }

    #[test]
    fn a_second_write_replaces_documents_of_its_own_type() {
        let image = minimal_image();
        let first = update_rcdata(&image, &[document(1, "old")]).unwrap();
        let second = update_rcdata(&first, &[document(1, "new"), document(2, "added")]).unwrap();
        assert_eq!(
            read_resource(&second, 10, 1).unwrap(),
            Some(b"new".to_vec())
        );
        assert_eq!(
            read_resource(&second, 10, 2).unwrap(),
            Some(b"added".to_vec())
        );
    }

    #[test]
    fn named_entries_survive_a_write() {
        let image = minimal_image();
        let mut tree = read_tree(&image, &layout(&image).unwrap()).unwrap();
        tree.0
            .entry(Key::Id(10))
            .or_default()
            .entry(Key::Name(vec![b'X' as u16]))
            .or_default()
            .push(Data {
                lang: 0,
                codepage: 0,
                bytes: b"named".to_vec(),
            });
        let added = append_section(&image, &layout(&image).unwrap(), &tree).unwrap();
        let updated = update_rcdata(&added, &[document(1, "index")]).unwrap();
        let layout = layout(&updated).unwrap();
        let tree = read_tree(&updated, &layout).unwrap();
        let names = tree.0.get(&Key::Id(10)).unwrap();
        assert_eq!(
            names.get(&Key::Name(vec![b'X' as u16])).unwrap()[0].bytes,
            b"named"
        );
        assert_eq!(
            read_resource(&updated, 10, 1).unwrap(),
            Some(b"index".to_vec())
        );
    }

    #[test]
    fn a_second_update_replaces_the_section_rather_than_appending() {
        let image = minimal_image();
        let composed = update_rcdata(&image, &[document(1, "index"), document(5, "blob")]).unwrap();
        assert_eq!(layout(&composed).unwrap().sections.len(), 2);
        let composed_layout = layout(&composed).unwrap();
        let stamped = update_icons(&composed, &[b"icon".to_vec()], b"group").unwrap();
        let stamped_layout = layout(&stamped).unwrap();
        assert_eq!(
            stamped_layout.sections.len(),
            2,
            "icons replace the bundle section; an orphaned copy would ship dead payload"
        );
        assert_eq!(
            stamped_layout.sections[1].virtual_address, 8192,
            "the replacement address comes from the sections that remain, not \
             the stale SizeOfImage of the removed one"
        );
        let restamped = update_icons(&stamped, &[b"icon2".to_vec()], b"group").unwrap();
        let restamped_layout = layout(&restamped).unwrap();
        assert_eq!(restamped_layout.sections.len(), 2);
        assert_eq!(
            restamped_layout.sections[1].virtual_address,
            stamped_layout.sections[1].virtual_address,
            "repeated updates reuse the section address; a stale SizeOfImage would \
             push each one a page past its file bytes and the image would fail to start"
        );
        assert_eq!(
            restamped_layout.size_of_image, stamped_layout.size_of_image,
            "repeated updates must not grow the virtual image while the file stays put"
        );
        assert!(
            stamped_layout.size_of_image <= composed_layout.size_of_image,
            "replacing a section never needs a larger image than appending it did"
        );
        assert_eq!(
            read_resource(&stamped, 10, 1).unwrap(),
            Some(b"index".to_vec())
        );
        assert_eq!(
            read_resource(&stamped, 10, 5).unwrap(),
            Some(b"blob".to_vec())
        );
        assert_eq!(
            read_resource(&stamped, RESOURCE_TYPE_ICON, 1).unwrap(),
            Some(b"icon".to_vec())
        );
        let restamped = update_icons(&stamped, &[b"icon2".to_vec()], b"group").unwrap();
        assert_eq!(layout(&restamped).unwrap().sections.len(), 2);
        assert_eq!(
            read_resource(&restamped, RESOURCE_TYPE_ICON, 1).unwrap(),
            Some(b"icon2".to_vec())
        );
    }

    #[test]
    fn foreign_bytes_and_signed_images_are_refused() {
        assert!(matches!(
            update_rcdata(b"not an image", &[document(1, "x")]),
            Err(PeError::Invalid)
        ));
        assert!(matches!(
            read_resource(b"not an image", 10, 1),
            Err(PeError::Invalid)
        ));
        let mut signed = minimal_image();
        let directory_base = 0x58 + 112;
        signed[directory_base + 4 * 8..directory_base + 4 * 8 + 4]
            .copy_from_slice(&0x5000u32.to_le_bytes());
        signed[directory_base + 4 * 8 + 4..directory_base + 4 * 8 + 8]
            .copy_from_slice(&0x100u32.to_le_bytes());
        assert!(matches!(
            update_rcdata(&signed, &[document(1, "x")]),
            Err(PeError::Invalid)
        ));
    }

    #[test]
    fn a_header_without_room_for_a_section_is_refused() {
        let mut image = minimal_image();
        image[0x58 + 60..0x58 + 64].copy_from_slice(&0x170u32.to_le_bytes());
        assert!(matches!(
            update_rcdata(&image, &[document(1, "x")]),
            Err(PeError::HeaderFull)
        ));
    }
}
