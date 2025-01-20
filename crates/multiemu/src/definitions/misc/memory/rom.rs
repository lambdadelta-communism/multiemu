use crate::{
    component::{memory::MemoryComponent, Component, FromConfig},
    machine::ComponentBuilder,
    memory::{PreviewMemoryRecord, ReadMemoryRecord, WriteMemoryRecord, VALID_ACCESS_SIZES},
    rom::{
        id::RomId,
        manager::{RomManager, RomRequirement},
    },
};
use rangemap::RangeMap;
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    ops::Range,
    sync::{Arc, Mutex},
};

#[derive(Debug)]
pub struct RomMemoryConfig {
    pub rom: RomId,
    // The maximum word size
    pub max_word_size: u8,
    // Memory region this buffer will be mapped to
    pub assigned_range: Range<usize>,
}

impl Default for RomMemoryConfig {
    fn default() -> Self {
        Self {
            rom: RomId::default(),
            max_word_size: 8,
            assigned_range: 0..0,
        }
    }
}

pub struct RomMemory {
    config: RomMemoryConfig,
    rom: Mutex<File>,
    rom_manager: Arc<RomManager>,
}

impl Component for RomMemory {
    fn reset(&self) {
        let rom_file = self
            .rom_manager
            .open(self.config.rom, RomRequirement::Required)
            .unwrap();

        *self.rom.lock().unwrap() = rom_file;
    }
}

impl FromConfig for RomMemory {
    type Config = RomMemoryConfig;

    fn from_config(component_builder: &mut ComponentBuilder<Self>, config: Self::Config) {
        let rom_manager = component_builder.machine().rom_manager();

        let rom_file = rom_manager
            .open(config.rom, RomRequirement::Required)
            .unwrap();

        component_builder.set_component(Self {
            config,
            rom: Mutex::new(rom_file),
            rom_manager,
        });
    }
}

impl MemoryComponent for RomMemory {
    fn read_memory(
        &self,
        address: usize,
        buffer: &mut [u8],
        errors: &mut RangeMap<usize, ReadMemoryRecord>,
    ) {
        debug_assert!(
            VALID_ACCESS_SIZES.contains(&buffer.len()),
            "Invalid memory access size {}",
            buffer.len()
        );

        let affected_range = address..address + buffer.len();

        if buffer.len() > self.config.max_word_size as usize {
            errors.insert(affected_range.clone(), ReadMemoryRecord::Denied);
        }

        let address = address - self.config.assigned_range.start;
        let mut rom_guard = self.rom.lock().unwrap();

        rom_guard.seek(SeekFrom::Start(address as u64)).unwrap();
        rom_guard.read_exact(buffer).unwrap();
    }

    fn write_memory(
        &self,
        address: usize,
        buffer: &[u8],
        errors: &mut RangeMap<usize, WriteMemoryRecord>,
    ) {
        debug_assert!(
            VALID_ACCESS_SIZES.contains(&(buffer.len())),
            "Invalid memory access size {}",
            buffer.len()
        );
        errors.insert(address..address + buffer.len(), WriteMemoryRecord::Denied);
    }

    fn preview_memory(
        &self,
        address: usize,
        buffer: &mut [u8],
        _errors: &mut RangeMap<usize, PreviewMemoryRecord>,
    ) {
        let address = address - self.config.assigned_range.start;

        let mut rom_guard = self.rom.lock().unwrap();

        rom_guard.seek(SeekFrom::Start(address as u64)).unwrap();
        rom_guard.read_exact(buffer).unwrap();
    }
}
