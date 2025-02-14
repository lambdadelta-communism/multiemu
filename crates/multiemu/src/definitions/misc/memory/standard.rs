use crate::{
    component::{memory::MemoryComponent, Component, FromConfig},
    machine::ComponentBuilder,
    memory::{AddressSpaceId, ReadMemoryRecord, WriteMemoryRecord, VALID_ACCESS_SIZES},
    rom::{
        id::RomId,
        manager::{RomManager, RomRequirement},
    },
};
use rand::RngCore;
use rangemap::RangeMap;
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use serde::{Deserialize, Serialize};
use std::{
    borrow::Cow,
    io::{Read, Write},
    ops::Range,
    sync::{Arc, Mutex},
};

const CHUNK_SIZE: usize = 4096;

#[derive(Debug)]
pub enum StandardMemoryInitialContents {
    Value {
        value: u8,
    },
    Array {
        offset: usize,
        value: Cow<'static, [u8]>,
    },
    Rom {
        rom_id: RomId,
        offset: usize,
    },
    Random,
}

#[derive(Debug)]
pub struct StandardMemoryConfig {
    // If the buffer is readable
    pub readable: bool,
    // If the buffer is writable
    pub writable: bool,
    // The maximum word size
    pub max_word_size: usize,
    // Memory region this buffer will be mapped to
    pub assigned_range: Range<usize>,
    /// Address space this exists on
    pub assigned_address_space: AddressSpaceId,
    // Initial contents
    pub initial_contents: StandardMemoryInitialContents,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StandardMemorySnapshot {
    pub memory: Vec<u8>,
}

#[derive(Debug)]
pub struct StandardMemory {
    config: StandardMemoryConfig,
    buffer: Vec<Mutex<[u8; CHUNK_SIZE]>>,
    rom_manager: Arc<RomManager>,
}

impl Component for StandardMemory {
    fn reset(&self) {
        self.initialize_buffer();
    }

    fn save_snapshot(&self) -> rmpv::Value {
        let mut memory = Vec::new();

        for chunk in self.buffer.iter() {
            let chunk_guard = chunk.lock().unwrap();
            memory.write_all(chunk_guard.as_slice()).unwrap();
        }

        let state = StandardMemorySnapshot { memory };

        rmpv::ext::to_value(&state).unwrap()
    }

    fn load_snapshot(&self, state: rmpv::Value) {
        let state = rmpv::ext::from_value::<StandardMemorySnapshot>(state).unwrap();

        assert_eq!(state.memory.len(), self.config.assigned_range.len());

        // This also does size validation
        for (src, dest) in state.memory.chunks(4096).zip(self.buffer.iter()) {
            let mut dest_guard = dest.lock().unwrap();
            dest_guard[..src.len()].copy_from_slice(src);
        }
    }
}

impl FromConfig for StandardMemory {
    type Config = StandardMemoryConfig;

    fn from_config(component_builder: &mut ComponentBuilder<Self>, config: Self::Config) {
        assert!(
            VALID_ACCESS_SIZES.contains(&config.max_word_size),
            "Invalid word size"
        );
        assert!(
            !config.assigned_range.is_empty(),
            "Memory assigned must be non-empty"
        );

        let buffer_size = config.assigned_range.len();
        let chunks_needed = buffer_size.div_ceil(CHUNK_SIZE);
        let buffer = Vec::from_iter(
            std::iter::repeat([0; CHUNK_SIZE])
                .take(chunks_needed)
                .map(Mutex::new),
        );
        let assigned_range = config.assigned_range.clone();
        let assigned_address_space = config.assigned_address_space;

        let me = Self {
            config,
            buffer: buffer.into_iter().collect(),
            rom_manager: component_builder.machine().rom_manager.clone(),
        };
        me.initialize_buffer();

        component_builder
            .set_component(me)
            .set_memory([(assigned_address_space, assigned_range)]);
    }
}

impl MemoryComponent for StandardMemory {
    fn read_memory(
        &self,
        address: usize,
        buffer: &mut [u8],
        _address_space: AddressSpaceId,
        errors: &mut RangeMap<usize, ReadMemoryRecord>,
    ) {
        debug_assert!(
            VALID_ACCESS_SIZES.contains(&buffer.len()),
            "Invalid memory access size {}",
            buffer.len()
        );

        if !self.config.readable {
            errors.insert(address..address + buffer.len(), ReadMemoryRecord::Denied);
        }

        let requested_range = address - self.config.assigned_range.start
            ..address - self.config.assigned_range.start + buffer.len();
        let invalid_before_range = address..self.config.assigned_range.start;
        let invalid_after_range = self.config.assigned_range.end..address + buffer.len();

        if !invalid_after_range.is_empty() || !invalid_before_range.is_empty() {
            errors.extend(
                [invalid_after_range, invalid_before_range]
                    .into_iter()
                    .filter_map(|range| {
                        if !range.is_empty() {
                            Some((range, ReadMemoryRecord::Denied))
                        } else {
                            None
                        }
                    }),
            );
        }

        if !errors.is_empty() {
            return;
        }

        let start_chunk = requested_range.start / CHUNK_SIZE;
        let end_chunk = requested_range.end.div_ceil(CHUNK_SIZE);

        let mut buffer_offset = 0;

        for chunk_index in start_chunk..end_chunk {
            let chunk = &self.buffer[chunk_index];

            let chunk_start = if chunk_index == start_chunk {
                requested_range.start % CHUNK_SIZE
            } else {
                0
            };

            let chunk_end = if chunk_index == end_chunk - 1 {
                // If we're in the last chunk, handle the exact range end
                if requested_range.end % CHUNK_SIZE == 0 && requested_range.end != 0 {
                    CHUNK_SIZE
                } else {
                    requested_range.end % CHUNK_SIZE
                }
            } else {
                CHUNK_SIZE
            };

            // Lock the chunk and read the relevant part
            let locked_chunk = chunk.lock().unwrap();
            buffer[buffer_offset..buffer_offset + chunk_end - chunk_start]
                .copy_from_slice(&locked_chunk[chunk_start..chunk_end]);

            buffer_offset += chunk_end - chunk_start;

            if buffer_offset >= buffer.len() {
                break;
            }
        }
    }

    fn write_memory(
        &self,
        address: usize,
        buffer: &[u8],
        _address_space: AddressSpaceId,
        errors: &mut RangeMap<usize, WriteMemoryRecord>,
    ) {
        debug_assert!(
            VALID_ACCESS_SIZES.contains(&buffer.len()),
            "Invalid memory access size {}",
            buffer.len()
        );

        if !self.config.writable {
            errors.insert(address..address + buffer.len(), WriteMemoryRecord::Denied);
        }

        let invalid_before_range = address..self.config.assigned_range.start;
        let invalid_after_range = self.config.assigned_range.end..address + buffer.len();

        if !invalid_after_range.is_empty() || !invalid_before_range.is_empty() {
            errors.extend(
                [invalid_after_range, invalid_before_range]
                    .into_iter()
                    .filter_map(|range| {
                        if !range.is_empty() {
                            Some((range, WriteMemoryRecord::Denied))
                        } else {
                            None
                        }
                    }),
            );
        }

        if !errors.is_empty() {
            return;
        }

        // Shoved off in a helper function to prevent duplicated logic
        self.write_internal(address, buffer);
    }
}

impl StandardMemory {
    /// Writes unchecked internally
    fn write_internal(&self, address: usize, buffer: &[u8]) {
        let requested_range = address - self.config.assigned_range.start
            ..address - self.config.assigned_range.start + buffer.len();

        let start_chunk = requested_range.start / CHUNK_SIZE;
        let end_chunk = requested_range.end.div_ceil(CHUNK_SIZE);

        let mut buffer_offset = 0;

        for chunk_index in start_chunk..end_chunk {
            let chunk = &self.buffer[chunk_index];

            let chunk_start = if chunk_index == start_chunk {
                requested_range.start % CHUNK_SIZE
            } else {
                0
            };

            let chunk_end = if chunk_index == end_chunk - 1 {
                // If we're in the last chunk, handle the exact range end
                if requested_range.end % CHUNK_SIZE == 0 && requested_range.end != 0 {
                    CHUNK_SIZE
                } else {
                    requested_range.end % CHUNK_SIZE
                }
            } else {
                CHUNK_SIZE
            };

            let mut locked_chunk = chunk.lock().unwrap();
            locked_chunk[chunk_start..chunk_end]
                .copy_from_slice(&buffer[buffer_offset..buffer_offset + chunk_end - chunk_start]);

            buffer_offset += chunk_end - chunk_start;

            if buffer_offset >= buffer.len() {
                break;
            }
        }
    }

    fn initialize_buffer(&self) {
        let internal_buffer_size = self.config.assigned_range.len();

        // HACK: This overfills the buffer for ease of programming, but its ok because the actual mmu doesn't allow accesses out at runtime
        match &self.config.initial_contents {
            StandardMemoryInitialContents::Value { value } => {
                self.buffer
                    .par_iter()
                    .for_each(|chunk| chunk.lock().unwrap().fill(*value));
            }
            StandardMemoryInitialContents::Random => {
                self.buffer
                    .par_iter()
                    .for_each(|chunk| rand::rng().fill_bytes(chunk.lock().unwrap().as_mut_slice()));
            }
            StandardMemoryInitialContents::Array { value, offset } => {
                self.write_internal(*offset, value);
            }
            StandardMemoryInitialContents::Rom { rom_id, offset } => {
                let mut rom_file = self
                    .rom_manager
                    .open(*rom_id, RomRequirement::Required)
                    .unwrap();

                let mut total_read = 0;
                let mut buffer = [0; 4096];

                while total_read < internal_buffer_size {
                    let remaining_space = internal_buffer_size - total_read;
                    let amount_to_read = remaining_space.min(buffer.len());
                    let amount = rom_file
                        .read(&mut buffer[..amount_to_read])
                        .expect("Could not read rom");

                    if amount == 0 {
                        break;
                    }

                    total_read += amount;

                    let write_size = remaining_space.min(amount);
                    self.write_internal(*offset + total_read - amount, &buffer[..write_size]);
                }
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{machine::Machine, rom::system::GameSystem};

    const ADDRESS_SPACE: AddressSpaceId = 0;

    #[test]
    fn initialization() {
        let rom_manager = Arc::new(RomManager::new(None).unwrap());
        let machine = Machine::build(GameSystem::Unknown, rom_manager.clone())
            .insert_bus(ADDRESS_SPACE, 64)
            .build_component::<StandardMemory>(StandardMemoryConfig {
                max_word_size: 8,
                readable: true,
                writable: true,
                assigned_range: 0..4,
                assigned_address_space: ADDRESS_SPACE,
                initial_contents: StandardMemoryInitialContents::Value { value: 0xff },
            })
            .0
            .build();
        let mut buffer = [0; 4];

        machine
            .memory_translation_table
            .read(0, &mut buffer, ADDRESS_SPACE)
            .unwrap();
        assert_eq!(buffer, [0xff; 4]);

        let machine = Machine::build(GameSystem::Unknown, rom_manager.clone())
            .insert_bus(ADDRESS_SPACE, 64)
            .build_component::<StandardMemory>(StandardMemoryConfig {
                max_word_size: 8,
                readable: true,
                writable: true,
                assigned_range: 0..4,
                assigned_address_space: ADDRESS_SPACE,
                initial_contents: StandardMemoryInitialContents::Array {
                    value: Cow::Borrowed(&[0xff; 4]),
                    offset: 0,
                },
            })
            .0
            .build();
        let mut buffer = [0; 4];

        machine
            .memory_translation_table
            .read(0, &mut buffer, ADDRESS_SPACE)
            .unwrap();
        assert_eq!(buffer, [0xff; 4]);
    }

    #[test]
    fn basic_read() {
        let rom_manager = Arc::new(RomManager::new(None).unwrap());
        let machine = Machine::build(GameSystem::Unknown, rom_manager)
            .insert_bus(ADDRESS_SPACE, 64)
            .build_component::<StandardMemory>(StandardMemoryConfig {
                max_word_size: 8,
                readable: true,
                writable: true,
                assigned_range: 0..0x10000,
                assigned_address_space: ADDRESS_SPACE,
                initial_contents: StandardMemoryInitialContents::Value { value: 0xff },
            })
            .0
            .build();
        let mut buffer = [0; 8];

        machine
            .memory_translation_table
            .read(0, &mut buffer, ADDRESS_SPACE)
            .unwrap();
        assert_eq!(buffer, [0xff; 8]);
    }

    #[test]
    fn basic_write() {
        let rom_manager = Arc::new(RomManager::new(None).unwrap());
        let machine = Machine::build(GameSystem::Unknown, rom_manager)
            .insert_bus(ADDRESS_SPACE, 64)
            .build_component::<StandardMemory>(StandardMemoryConfig {
                max_word_size: 8,
                readable: true,
                writable: true,
                assigned_range: 0..0x10000,
                assigned_address_space: ADDRESS_SPACE,
                initial_contents: StandardMemoryInitialContents::Value { value: 0xff },
            })
            .0
            .build();
        let buffer = [0; 8];

        machine
            .memory_translation_table
            .write(0, &buffer, ADDRESS_SPACE)
            .unwrap();
    }

    #[test]
    fn basic_read_write() {
        let rom_manager = Arc::new(RomManager::new(None).unwrap());
        let machine = Machine::build(GameSystem::Unknown, rom_manager)
            .insert_bus(ADDRESS_SPACE, 64)
            .build_component::<StandardMemory>(StandardMemoryConfig {
                max_word_size: 8,
                readable: true,
                writable: true,
                assigned_range: 0..0x10000,
                assigned_address_space: ADDRESS_SPACE,
                initial_contents: StandardMemoryInitialContents::Value { value: 0xff },
            })
            .0
            .build();
        let mut buffer = [0xff; 8];

        machine
            .memory_translation_table
            .write(0, &buffer, ADDRESS_SPACE)
            .unwrap();
        buffer.fill(0);
        machine
            .memory_translation_table
            .read(0, &mut buffer, ADDRESS_SPACE)
            .unwrap();
        assert_eq!(buffer, [0xff; 8]);
    }

    #[test]
    fn extensive() {
        let rom_manager = Arc::new(RomManager::new(None).unwrap());
        let machine = Machine::build(GameSystem::Unknown, rom_manager)
            .insert_bus(ADDRESS_SPACE, 64)
            .build_component::<StandardMemory>(StandardMemoryConfig {
                max_word_size: 8,
                readable: true,
                writable: true,
                assigned_range: 0..0x10000,
                assigned_address_space: ADDRESS_SPACE,
                initial_contents: StandardMemoryInitialContents::Value { value: 0xff },
            })
            .0
            .build();
        let mut buffer = [0xff; 1];

        for i in 0..0x10000 {
            machine
                .memory_translation_table
                .write(i, &buffer, ADDRESS_SPACE)
                .unwrap();
            buffer.fill(0x00);
            machine
                .memory_translation_table
                .read(i, &mut buffer, ADDRESS_SPACE)
                .unwrap();
            assert_eq!(buffer, [0xff; 1]);
        }
    }
}
