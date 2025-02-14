use super::Chip8Kind;
use crate::{
    component::{
        display::DisplayComponent, schedulable::SchedulableComponent, Component, FromConfig,
    },
    machine::ComponentBuilder,
    runtime::rendering_backend::{DisplayComponentFramebuffer, DisplayComponentInitializationData},
};
use bitvec::{order::Msb0, view::BitView};
use nalgebra::{DMatrix, DMatrixViewMut, Point2, Vector2};
use num::rational::Ratio;
use palette::Srgba;
use serde::{Deserialize, Serialize};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, OnceLock,
};

#[cfg(platform_desktop)]
mod desktop;
#[cfg(graphics_vulkan)]
use desktop::vulkan::VulkanState;

mod software;
use software::SoftwareState;

#[derive(Debug)]
#[non_exhaustive]
enum InternalState {
    #[cfg(graphics_vulkan)]
    Vulkan(VulkanState),
    Software(SoftwareState),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Chip8DisplaySnapshot {
    screen_buffer: DMatrix<Srgba<u8>>,
}

#[derive(Debug)]
pub struct Chip8Display {
    config: Chip8DisplayConfig,
    state: OnceLock<InternalState>,
    modified: AtomicBool,
}

impl Chip8Display {
    pub fn draw_sprite(&self, position: Point2<u8>, sprite: &[u8]) -> bool {
        tracing::trace!(
            "Drawing sprite at position {} of dimensions 8x{}",
            position,
            sprite.len()
        );

        let position = match self.config.kind {
            Chip8Kind::Chip8 | Chip8Kind::Chip48 => Point2::new(position.x % 63, position.y % 31),
            Chip8Kind::SuperChip8 => todo!(),
            _ => todo!(),
        };

        self.modified.store(true, Ordering::Relaxed);

        match self.state.get() {
            #[cfg(graphics_vulkan)]
            Some(InternalState::Vulkan(vulkan_state)) => vulkan_state.draw_sprite(position, sprite),
            Some(InternalState::Software(software_state)) => {
                software_state.draw_sprite(position, sprite)
            }
            _ => panic!("Internal state not initialized"),
        }
    }

    pub fn clear_display(&self) {
        tracing::trace!("Clearing display");

        match self.state.get() {
            #[cfg(graphics_vulkan)]
            Some(InternalState::Vulkan(vulkan_state)) => vulkan_state.clear_display(),
            Some(InternalState::Software(software_state)) => software_state.clear_display(),
            _ => panic!("Internal state not initialized"),
        }
    }
}

impl Component for Chip8Display {
    fn reset(&self) {
        self.clear_display();
    }

    fn save_snapshot(&self) -> rmpv::Value {
        let display_buffer = match self.state.get() {
            #[cfg(graphics_vulkan)]
            Some(InternalState::Vulkan(vulkan_state)) => vulkan_state.save_screen_contents(),
            Some(InternalState::Software(software_state)) => software_state.save_screen_contents(),
            _ => panic!("Internal state not initialized"),
        };

        rmpv::ext::to_value(Chip8DisplaySnapshot {
            screen_buffer: display_buffer,
        })
        .unwrap()
    }

    fn load_snapshot(&self, state: rmpv::Value) {
        let snapshot: Chip8DisplaySnapshot = rmpv::ext::from_value(state).unwrap();

        match self.state.get() {
            #[cfg(graphics_vulkan)]
            Some(InternalState::Vulkan(vulkan_state)) => {
                vulkan_state.load_screen_contents(snapshot.screen_buffer);
            }
            Some(InternalState::Software(software_state)) => {
                software_state.load_screen_contents(snapshot.screen_buffer);
            }
            _ => panic!("Internal state not initialized"),
        }
    }
}

#[derive(Debug)]
pub struct Chip8DisplayConfig {
    pub kind: Chip8Kind,
}

impl FromConfig for Chip8Display {
    type Config = Chip8DisplayConfig;

    fn from_config(component_builder: &mut ComponentBuilder<Self>, config: Self::Config) {
        component_builder
            .set_component(Chip8Display {
                config,
                state: OnceLock::default(),
                modified: AtomicBool::new(false),
            })
            .set_schedulable(Ratio::from_integer(60), [], [])
            .set_display();
    }
}

trait Chip8DisplayImplementation {
    fn draw_sprite(&self, position: Point2<u8>, sprite: &[u8]) -> bool;
    fn clear_display(&self);
    fn save_screen_contents(&self) -> DMatrix<Srgba<u8>>;
    fn load_screen_contents(&self, buffer: DMatrix<Srgba<u8>>);
    fn get_framebuffer(&self) -> DisplayComponentFramebuffer;
    fn commit_display(&self);
}

impl SchedulableComponent for Chip8Display {
    fn run(&self, _period: u64) {
        // Only update it once and if the thing is actually updated
        if self.modified.swap(false, Ordering::Relaxed) {
            match self.state.get() {
                Some(InternalState::Software(software_state)) => {
                    software_state.commit_display();
                }
                #[cfg(graphics_vulkan)]
                Some(InternalState::Vulkan(vulkan_state)) => {
                    vulkan_state.commit_display();
                }
                _ => panic!("Internal state not initialized"),
            }
        }
    }
}

impl DisplayComponent for Chip8Display {
    fn set_display_data(&self, initialization_data: DisplayComponentInitializationData) {
        let _ = self.state.set(match initialization_data {
            DisplayComponentInitializationData::Software => {
                let framebuffer = DMatrix::from_element(64, 32, Srgba::new(0, 0, 0, 255));
                InternalState::Software(SoftwareState {
                    framebuffer: Arc::new(Mutex::new(framebuffer)),
                })
            }
            #[cfg(graphics_vulkan)]
            DisplayComponentInitializationData::Vulkan(initialization_data) => {
                use vulkano::buffer::Buffer;
                use vulkano::buffer::BufferCreateInfo;
                use vulkano::buffer::BufferUsage;
                use vulkano::format::Format;
                use vulkano::image::Image;
                use vulkano::image::ImageCreateInfo;
                use vulkano::image::ImageType;
                use vulkano::image::ImageUsage;
                use vulkano::memory::allocator::AllocationCreateInfo;
                use vulkano::memory::allocator::MemoryTypeFilter;

                let staging_buffer = Buffer::from_iter(
                    initialization_data.memory_allocator.clone(),
                    BufferCreateInfo {
                        usage: BufferUsage::TRANSFER_SRC,
                        ..Default::default()
                    },
                    AllocationCreateInfo {
                        memory_type_filter: MemoryTypeFilter::HOST_RANDOM_ACCESS,
                        ..Default::default()
                    },
                    vec![Srgba::new(0, 0, 0, 0xff); 64 * 32],
                )
                .unwrap();

                let render_image = Image::new(
                    initialization_data.memory_allocator.clone(),
                    ImageCreateInfo {
                        image_type: ImageType::Dim2d,
                        format: Format::R8G8B8A8_SRGB,
                        extent: [64, 32, 1],
                        usage: ImageUsage::TRANSFER_SRC
                            | ImageUsage::TRANSFER_DST
                            | ImageUsage::SAMPLED,
                        ..Default::default()
                    },
                    AllocationCreateInfo::default(),
                )
                .unwrap();

                InternalState::Vulkan(VulkanState {
                    queue: initialization_data.queue,
                    command_buffer_allocator: initialization_data.command_buffer_allocator,
                    staging_buffer,
                    render_image,
                })
            }
        });
    }

    fn get_framebuffer(&self) -> DisplayComponentFramebuffer {
        match self.state.get() {
            Some(InternalState::Software(software_state)) => software_state.get_framebuffer(),
            #[cfg(graphics_vulkan)]
            Some(InternalState::Vulkan(vulkan_state)) => vulkan_state.get_framebuffer(),
            _ => panic!("Internal state not initialized"),
        }
    }
}

fn draw_sprite_common(
    position: Point2<u8>,
    sprite: &[u8],
    mut framebuffer: DMatrixViewMut<'_, Srgba<u8>>,
) -> bool {
    let mut collided = false;
    let position = position.cast();

    for (y, sprite_row) in sprite.view_bits::<Msb0>().chunks(8).enumerate() {
        for (x, sprite_pixel) in sprite_row.iter().enumerate() {
            let coord = position + Vector2::new(x, y);

            if coord.x >= 64 || coord.y >= 32 {
                continue;
            }

            let old_sprite_pixel =
                framebuffer[(coord.x, coord.y)] == Srgba::new(255, 255, 255, 255);

            if *sprite_pixel && old_sprite_pixel {
                collided = true;
            }

            framebuffer[(coord.x, coord.y)] = if *sprite_pixel ^ old_sprite_pixel {
                Srgba::new(255, 255, 255, 255)
            } else {
                Srgba::new(0, 0, 0, 255)
            };
        }
    }

    collided
}
