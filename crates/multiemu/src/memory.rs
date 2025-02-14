use crate::{component::ComponentId, machine::component_store::ComponentStore};
use arrayvec::ArrayVec;
use bitvec::{field::BitField, order::Lsb0, view::BitView};
use rangemap::RangeMap;
use std::{collections::HashMap, ops::Range, sync::Arc};
use thiserror::Error;

pub const VALID_ACCESS_SIZES: &[usize] = &[1, 2, 4, 8];

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ReadMemoryOperationErrorFailureType {
    Denied,
    OutOfBus,
}

#[derive(Error, Debug)]
#[error("Read operation failed: {0:#?}")]
pub struct ReadMemoryOperationError(RangeMap<usize, ReadMemoryOperationErrorFailureType>);

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum WriteMemoryOperationErrorFailureType {
    Denied,
    OutOfBus,
}

#[derive(Error, Debug)]
#[error("Write operation failed: {0:#?}")]
pub struct WriteMemoryOperationError(RangeMap<usize, WriteMemoryOperationErrorFailureType>);

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum PreviewMemoryOperationErrorFailureType {
    Denied,
    OutOfBus,
    Impossible,
}

#[derive(Error, Debug)]
#[error("Preview operation failed (this really shouldn't be thrown): {0:#?}")]
pub struct PreviewMemoryOperationError(RangeMap<usize, PreviewMemoryOperationErrorFailureType>);

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReadMemoryRecord {
    /// Memory could not be read
    Denied,
    /// Memory redirects somewhere else
    Redirect { address: usize },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WriteMemoryRecord {
    /// Memory could not be written
    Denied,
    /// Memory redirects somewhere else
    Redirect { address: usize },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PreviewMemoryRecord {
    /// Memory denied
    Denied,
    /// Memory redirects somewhere else
    Redirect {
        address: usize,
    },
    // Memory here can't be read without an intense calculation or a state change
    Impossible,
}

const MAX_ACCESS_SIZE: u8 = const {
    let mut max = VALID_ACCESS_SIZES[0];
    let mut index = 0;
    while index < VALID_ACCESS_SIZES.len() {
        if VALID_ACCESS_SIZES[index] > max {
            max = VALID_ACCESS_SIZES[index];
        }
        index += 1;
    }

    max as u8
};

pub type AddressSpaceId = u8;

#[derive(Debug)]
pub struct BusInfo {
    population: RangeMap<usize, ComponentId>,
    width: u8,
}

#[derive(Default, Debug)]
pub struct MemoryTranslationTable {
    busses: HashMap<AddressSpaceId, BusInfo>,
    component_store: Option<Arc<ComponentStore>>,
}

impl MemoryTranslationTable {
    pub fn insert_bus(&mut self, id: AddressSpaceId, width: u8) {
        self.busses.entry(id).or_insert_with(|| BusInfo {
            population: RangeMap::default(),
            width,
        });
    }

    pub fn insert_component(
        &mut self,
        id: AddressSpaceId,
        component_id: ComponentId,
        ranges: impl IntoIterator<Item = Range<usize>>,
    ) {
        self.busses
            .get_mut(&id)
            .expect("Bus must be initialized before inserting component")
            .population
            .extend(ranges.into_iter().map(|range| (range, component_id)));
    }

    pub fn set_component_store(&mut self, component_store: Arc<ComponentStore>) {
        self.component_store = Some(component_store);
    }

    pub fn address_spaces(&self) -> u8 {
        self.busses
            .len()
            .try_into()
            .expect("Too many address spaces!")
    }

    /// Step through the memory translation table to fill the buffer with data
    ///
    /// Contents of the buffer upon failure are usually component specific
    #[inline]
    pub fn read(
        &self,
        address: usize,
        buffer: &mut [u8],
        address_space: AddressSpaceId,
    ) -> Result<(), ReadMemoryOperationError> {
        debug_assert!(
            VALID_ACCESS_SIZES.contains(&buffer.len()),
            "Invalid memory access size {}",
            buffer.len()
        );

        let bus_info = self
            .busses
            .get(&address_space)
            .expect("Non existant address space");

        // Cut off address
        let address = address.view_bits::<Lsb0>()[..bus_info.width as usize].load_le::<usize>();

        let mut needed_accesses =
            ArrayVec::<_, { MAX_ACCESS_SIZE as usize }>::from_iter([(address, 0..buffer.len())]);

        while let Some((address, buffer_subrange)) = needed_accesses.pop() {
            let accessing_range =
                (buffer_subrange.start + address)..(buffer_subrange.end + address);

            for (component_assignment_range, component_id) in
                bus_info.population.overlapping(accessing_range.clone())
            {
                let mut errors = RangeMap::default();
                let component = self
                    .component_store
                    .as_ref()
                    .unwrap()
                    .get(*component_id)
                    .and_then(|table| table.as_memory.as_ref().map(|info| &info.component))
                    .unwrap();

                let overlap_start = accessing_range.start.max(component_assignment_range.start);
                let overlap_end = accessing_range.end.min(component_assignment_range.end);
                let overlap = overlap_start..overlap_end;

                component.read_memory(
                    overlap.start,
                    &mut buffer[buffer_subrange.clone()],
                    address_space,
                    &mut errors,
                );

                let mut detected_errors = RangeMap::default();

                for (range, error) in errors {
                    match error {
                        ReadMemoryRecord::Denied => {
                            detected_errors
                                .insert(range, ReadMemoryOperationErrorFailureType::Denied);
                        }
                        ReadMemoryRecord::Redirect {
                            address: redirect_address,
                        } => {
                            assert!(
                                !component_assignment_range.contains(&redirect_address),
                                "Component attempted to redirect to itself"
                            );

                            needed_accesses.push((
                                redirect_address,
                                (range.start - address)..(range.end - address),
                            ));
                        }
                    }
                }

                if !detected_errors.is_empty() {
                    return Err(ReadMemoryOperationError(detected_errors));
                }
            }
        }

        Ok(())
    }

    /// Step through the memory translation table to give a set of components the buffer
    ///
    /// Contents of the buffer upon failure are usually component specific
    #[inline]
    pub fn write(
        &self,
        address: usize,
        buffer: &[u8],
        address_space: AddressSpaceId,
    ) -> Result<(), WriteMemoryOperationError> {
        debug_assert!(
            VALID_ACCESS_SIZES.contains(&buffer.len()),
            "Invalid memory access size {}",
            buffer.len()
        );

        let bus_info = self
            .busses
            .get(&address_space)
            .expect("Non existant address space");

        let address = address.view_bits::<Lsb0>()[..bus_info.width as usize].load_le::<usize>();

        let mut needed_accesses =
            ArrayVec::<_, { MAX_ACCESS_SIZE as usize }>::from_iter([(address, 0..buffer.len())]);

        while let Some((address, buffer_subrange)) = needed_accesses.pop() {
            let accessing_range =
                (buffer_subrange.start + address)..(buffer_subrange.end + address);

            for (component_assignment_range, component_id) in
                bus_info.population.overlapping(accessing_range.clone())
            {
                let mut errors = RangeMap::default();
                let component = self
                    .component_store
                    .as_ref()
                    .unwrap()
                    .get(*component_id)
                    .and_then(|table| table.as_memory.as_ref().map(|info| &info.component))
                    .unwrap();

                let overlap_start = accessing_range.start.max(component_assignment_range.start);
                let overlap_end = accessing_range.end.min(component_assignment_range.end);
                let overlap = overlap_start..overlap_end;

                component.write_memory(
                    overlap.start,
                    &buffer[buffer_subrange.clone()],
                    address_space,
                    &mut errors,
                );

                let mut detected_errors = RangeMap::default();

                for (range, error) in errors {
                    match error {
                        WriteMemoryRecord::Denied => {
                            detected_errors
                                .insert(range, WriteMemoryOperationErrorFailureType::Denied);
                        }
                        WriteMemoryRecord::Redirect {
                            address: redirect_address,
                        } => {
                            assert!(
                                !component_assignment_range.contains(&redirect_address),
                                "Component attempted to redirect to itself"
                            );

                            needed_accesses.push((
                                redirect_address,
                                (range.start - address)..(range.end - address),
                            ));
                        }
                    }
                }

                if !detected_errors.is_empty() {
                    return Err(WriteMemoryOperationError(detected_errors));
                }
            }
        }

        Ok(())
    }

    #[inline]
    pub fn preview(
        &self,
        address: usize,
        buffer: &mut [u8],
        address_space: AddressSpaceId,
    ) -> Result<(), PreviewMemoryOperationError> {
        debug_assert!(
            VALID_ACCESS_SIZES.contains(&buffer.len()),
            "Invalid memory access size {}",
            buffer.len()
        );

        let bus_info = self
            .busses
            .get(&address_space)
            .expect("Non existant address space");

        let address = address.view_bits::<Lsb0>()[..bus_info.width as usize].load_le::<usize>();

        let mut needed_accesses =
            ArrayVec::<_, { MAX_ACCESS_SIZE as usize }>::from_iter([(address, 0..buffer.len())]);

        while let Some((address, buffer_subrange)) = needed_accesses.pop() {
            let accessing_range =
                (buffer_subrange.start + address)..(buffer_subrange.end + address);

            for (component_assignment_range, component_id) in
                bus_info.population.overlapping(accessing_range.clone())
            {
                let mut errors = RangeMap::default();
                let component = self
                    .component_store
                    .as_ref()
                    .unwrap()
                    .get(*component_id)
                    .and_then(|table| table.as_memory.as_ref().map(|info| &info.component))
                    .unwrap();

                let overlap_start = accessing_range.start.max(component_assignment_range.start);
                let overlap_end = accessing_range.end.min(component_assignment_range.end);
                let overlap = overlap_start..overlap_end;

                component.preview_memory(
                    overlap.start,
                    &mut buffer[buffer_subrange.clone()],
                    address_space,
                    &mut errors,
                );

                let mut detected_errors = RangeMap::default();

                for (range, error) in errors {
                    match error {
                        PreviewMemoryRecord::Denied => {
                            detected_errors
                                .insert(range, PreviewMemoryOperationErrorFailureType::Denied);
                        }
                        PreviewMemoryRecord::Redirect {
                            address: redirect_address,
                        } => {
                            assert!(
                                !component_assignment_range.contains(&redirect_address),
                                "Component attempted to redirect to itself"
                            );

                            needed_accesses.push((
                                redirect_address,
                                (range.start - address)..(range.end - address),
                            ));
                        }
                        PreviewMemoryRecord::Impossible => {
                            detected_errors
                                .insert(range, PreviewMemoryOperationErrorFailureType::Impossible);
                        }
                    }
                }

                if !detected_errors.is_empty() {
                    return Err(PreviewMemoryOperationError(detected_errors));
                }
            }
        }

        Ok(())
    }
}
