//! Vulkan backend. Every call into the driver is `unsafe` by nature; the handles never leave
//! this module, and each one is destroyed exactly once, in `Drop`.

use std::collections::HashMap;
use std::ffi::c_void;
use std::io::Cursor;

use ash::prelude::VkResult;
use ash::{Device, Entry, Instance, khr, vk};
use egui::epaint::{ImageDelta, Primitive, Vertex};
use egui::{ImageData, TextureFilter, TextureId, TextureOptions, TextureWrapMode};

use super::{Frame, Graphics};
use crate::window::NativeWindow;
use crate::{Result, bail, err};

const VERTEX_SHADER: &[u8] = include_bytes!("shaders/ui.vert.spv");
const FRAGMENT_SHADER: &[u8] = include_bytes!("shaders/ui.frag.spv");
const MOST_TEXTURES: u32 = 1024;
const SMALLEST_BUFFER: u64 = 1 << 16;

fn check<T>(what: &str, result: VkResult<T>) -> Result<T> {
    result.map_err(|code| err!("{what} failed: {code}"))
}

/// The instance and the surface on the overlay's window; outlives everything made from them.
struct Base {
    _entry: Entry,
    instance: Instance,
    surfaces: khr::surface::Instance,
    android: khr::android_surface::Instance,
    surface: vk::SurfaceKHR,
    window: *mut c_void,
}

impl Base {
    unsafe fn new(window: *mut c_void) -> Result<Self> {
        unsafe {
            let entry = Entry::load().map_err(|error| err!("Vulkan is not available: {error}"))?;
            let application = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_0);
            let extensions = [khr::surface::NAME.as_ptr(), khr::android_surface::NAME.as_ptr()];
            let info = vk::InstanceCreateInfo::default().application_info(&application).enabled_extension_names(&extensions);
            let instance = check("vkCreateInstance", entry.create_instance(&info, None))?;
            let (surfaces, android) = (khr::surface::Instance::new(&entry, &instance), khr::android_surface::Instance::new(&entry, &instance));
            let mut base = Self { _entry: entry, instance, surfaces, android, surface: vk::SurfaceKHR::null(), window: std::ptr::null_mut() };
            base.attach(window)?;
            Ok(base)
        }
    }

    unsafe fn attach(&mut self, window: *mut c_void) -> Result<()> {
        unsafe {
            self.surfaces.destroy_surface(std::mem::take(&mut self.surface), None);
            self.surface = check("vkCreateAndroidSurfaceKHR", self.android.create_android_surface(&vk::AndroidSurfaceCreateInfoKHR::default().window(window), None))?;
            self.window = window;
            Ok(())
        }
    }
}

impl Drop for Base {
    fn drop(&mut self) {
        unsafe {
            self.surfaces.destroy_surface(self.surface, None);
            self.instance.destroy_instance(None);
        }
    }
}

/// What a frame is drawn into: the swapchain and a framebuffer for each of its images.
#[derive(Default)]
struct Target {
    swapchain: vk::SwapchainKHR,
    extent: vk::Extent2D,
    views: Vec<vk::ImageView>,
    framebuffers: Vec<vk::Framebuffer>,
}

/// Memory the CPU writes mesh data into each frame; it grows and never shrinks.
struct Buffer {
    usage: vk::BufferUsageFlags,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    capacity: u64,
    mapped: *mut u8,
}

struct Texture {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    sampler: vk::Sampler,
    set: vk::DescriptorSet,
}

pub struct Vulkan {
    device: Device,
    swapchains: khr::swapchain::Device,
    physical: vk::PhysicalDevice,
    memory: vk::PhysicalDeviceMemoryProperties,
    family: u32,
    queue: vk::Queue,
    format: vk::SurfaceFormatKHR,
    render_pass: vk::RenderPass,
    set_layout: vk::DescriptorSetLayout,
    layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    descriptors: vk::DescriptorPool,
    pool: vk::CommandPool,
    commands: vk::CommandBuffer,
    acquired: vk::Semaphore,
    drawn: vk::Semaphore,
    idle: vk::Fence,
    target: Target,
    vertices: Buffer,
    indices: Buffer,
    textures: HashMap<TextureId, Texture>,
    max_texture_side: usize,
    base: Base,
}

impl Vulkan {
    pub fn new(window: &NativeWindow) -> Result<Self> {
        unsafe {
            let base = Base::new(window.as_ptr())?;
            let devices = check("vkEnumeratePhysicalDevices", base.instance.enumerate_physical_devices())?;
            let presents = |physical: vk::PhysicalDevice, family: u32| base.surfaces.get_physical_device_surface_support(physical, family, base.surface).unwrap_or(false);
            let candidates = devices
                .iter()
                .flat_map(|&physical| (0u32..).zip(base.instance.get_physical_device_queue_family_properties(physical)).map(move |(family, queues)| (physical, family, queues)));
            let Some((physical, family, _)) =
                candidates.into_iter().find(|&(physical, family, queues)| queues.queue_flags.contains(vk::QueueFlags::GRAPHICS) && presents(physical, family))
            else {
                bail!("no Vulkan device can draw to the overlay's window");
            };

            let queues = [vk::DeviceQueueCreateInfo::default().queue_family_index(family).queue_priorities(&[1.0])];
            let extensions = [khr::swapchain::NAME.as_ptr()];
            let info = vk::DeviceCreateInfo::default().queue_create_infos(&queues).enabled_extension_names(&extensions);
            let device = check("vkCreateDevice", base.instance.create_device(physical, &info, None))?;
            let limits = base.instance.get_physical_device_properties(physical).limits;

            let mut vulkan = Self {
                swapchains: khr::swapchain::Device::new(&base.instance, &device),
                memory: base.instance.get_physical_device_memory_properties(physical),
                queue: device.get_device_queue(family, 0),
                device,
                physical,
                family,
                format: vk::SurfaceFormatKHR::default(),
                render_pass: vk::RenderPass::null(),
                set_layout: vk::DescriptorSetLayout::null(),
                layout: vk::PipelineLayout::null(),
                pipeline: vk::Pipeline::null(),
                descriptors: vk::DescriptorPool::null(),
                pool: vk::CommandPool::null(),
                commands: vk::CommandBuffer::null(),
                acquired: vk::Semaphore::null(),
                drawn: vk::Semaphore::null(),
                idle: vk::Fence::null(),
                target: Target::default(),
                vertices: Buffer::empty(vk::BufferUsageFlags::VERTEX_BUFFER),
                indices: Buffer::empty(vk::BufferUsageFlags::INDEX_BUFFER),
                textures: HashMap::new(),
                max_texture_side: limits.max_image_dimension2_d as usize,
                base,
            };
            vulkan.prepare()?;
            vulkan.rebuild_target()?;
            let version = vulkan.base.instance.get_physical_device_properties(physical).api_version;
            log::info!("Vulkan {}.{} backend ready", vk::api_version_major(version), vk::api_version_minor(version));
            Ok(vulkan)
        }
    }

    /// Everything that does not depend on the size of the window.
    unsafe fn prepare(&mut self) -> Result<()> {
        unsafe {
            let device = &self.device;
            let pool = vk::CommandPoolCreateInfo::default().queue_family_index(self.family).flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
            self.pool = check("vkCreateCommandPool", device.create_command_pool(&pool, None))?;
            let commands = vk::CommandBufferAllocateInfo::default().command_pool(self.pool).level(vk::CommandBufferLevel::PRIMARY).command_buffer_count(1);
            self.commands = check("vkAllocateCommandBuffers", device.allocate_command_buffers(&commands))?[0];
            self.acquired = check("vkCreateSemaphore", device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None))?;
            self.drawn = check("vkCreateSemaphore", device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None))?;
            self.idle = check("vkCreateFence", device.create_fence(&vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED), None))?;

            let bindings = [vk::DescriptorSetLayoutBinding::default()
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT)];
            self.set_layout = check("vkCreateDescriptorSetLayout", device.create_descriptor_set_layout(&vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings), None))?;
            let (set_layouts, constants) = ([self.set_layout], [vk::PushConstantRange::default().stage_flags(vk::ShaderStageFlags::VERTEX).size(8)]);
            self.layout = check(
                "vkCreatePipelineLayout",
                device.create_pipeline_layout(&vk::PipelineLayoutCreateInfo::default().set_layouts(&set_layouts).push_constant_ranges(&constants), None),
            )?;
            let sizes = [vk::DescriptorPoolSize::default().ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER).descriptor_count(MOST_TEXTURES)];
            let descriptors = vk::DescriptorPoolCreateInfo::default().pool_sizes(&sizes).max_sets(MOST_TEXTURES).flags(vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET);
            self.descriptors = check("vkCreateDescriptorPool", device.create_descriptor_pool(&descriptors, None))?;

            // Colours are blended as they are, without conversion, like the other backends do.
            let formats = check("vkGetPhysicalDeviceSurfaceFormatsKHR", self.base.surfaces.get_physical_device_surface_formats(self.physical, self.base.surface))?;
            let plain = [vk::Format::R8G8B8A8_UNORM, vk::Format::B8G8R8A8_UNORM];
            self.format = plain
                .iter()
                .find_map(|&wanted| formats.iter().copied().find(|format| format.format == wanted))
                .ok_or_else(|| err!("the window offers no 8-bit RGBA format to Vulkan"))?;

            let attachments = [vk::AttachmentDescription::default()
                .format(self.format.format)
                .samples(vk::SampleCountFlags::TYPE_1)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
                .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
                .final_layout(vk::ImageLayout::PRESENT_SRC_KHR)];
            let colors = [vk::AttachmentReference::default().layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
            let subpasses = [vk::SubpassDescription::default().pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS).color_attachments(&colors)];
            let dependencies = [vk::SubpassDependency::default()
                .src_subpass(vk::SUBPASS_EXTERNAL)
                .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
                .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
                .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)];
            let pass = vk::RenderPassCreateInfo::default().attachments(&attachments).subpasses(&subpasses).dependencies(&dependencies);
            self.render_pass = check("vkCreateRenderPass", device.create_render_pass(&pass, None))?;
            self.pipeline = self.create_pipeline()?;
            Ok(())
        }
    }

    unsafe fn create_pipeline(&self) -> Result<vk::Pipeline> {
        unsafe {
            let device = &self.device;
            let module = |code: &[u8]| {
                let words = ash::util::read_spv(&mut Cursor::new(code)).map_err(|error| err!("bad shader: {error}"))?;
                check("vkCreateShaderModule", device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None))
            };
            let vertex = module(VERTEX_SHADER)?;
            let fragment = module(FRAGMENT_SHADER).inspect_err(|_| device.destroy_shader_module(vertex, None))?;
            let stages = [
                vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::VERTEX).module(vertex).name(c"main"),
                vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::FRAGMENT).module(fragment).name(c"main"),
            ];
            let bindings = [vk::VertexInputBindingDescription::default().stride(size_of::<Vertex>() as u32).input_rate(vk::VertexInputRate::VERTEX)];
            let attribute = |location, format, offset| vk::VertexInputAttributeDescription::default().location(location).format(format).offset(offset);
            let attributes = [attribute(0, vk::Format::R32G32_SFLOAT, 0), attribute(1, vk::Format::R32G32_SFLOAT, 8), attribute(2, vk::Format::R8G8B8A8_UNORM, 16)];
            let input = vk::PipelineVertexInputStateCreateInfo::default().vertex_binding_descriptions(&bindings).vertex_attribute_descriptions(&attributes);
            let assembly = vk::PipelineInputAssemblyStateCreateInfo::default().topology(vk::PrimitiveTopology::TRIANGLE_LIST);
            let viewport = vk::PipelineViewportStateCreateInfo::default().viewport_count(1).scissor_count(1);
            let raster = vk::PipelineRasterizationStateCreateInfo::default().polygon_mode(vk::PolygonMode::FILL).cull_mode(vk::CullModeFlags::NONE).line_width(1.0);
            let samples = vk::PipelineMultisampleStateCreateInfo::default().rasterization_samples(vk::SampleCountFlags::TYPE_1);
            // Premultiplied alpha: the source is added to what shows through it.
            let blends = [vk::PipelineColorBlendAttachmentState::default()
                .blend_enable(true)
                .src_color_blend_factor(vk::BlendFactor::ONE)
                .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                .color_blend_op(vk::BlendOp::ADD)
                .src_alpha_blend_factor(vk::BlendFactor::ONE)
                .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                .alpha_blend_op(vk::BlendOp::ADD)
                .color_write_mask(vk::ColorComponentFlags::RGBA)];
            let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blends);
            let dynamics = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
            let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamics);
            let info = vk::GraphicsPipelineCreateInfo::default()
                .stages(&stages)
                .vertex_input_state(&input)
                .input_assembly_state(&assembly)
                .viewport_state(&viewport)
                .rasterization_state(&raster)
                .multisample_state(&samples)
                .color_blend_state(&blend)
                .dynamic_state(&dynamic)
                .layout(self.layout)
                .render_pass(self.render_pass);
            let pipelines = device.create_graphics_pipelines(vk::PipelineCache::null(), &[info], None);
            device.destroy_shader_module(vertex, None);
            device.destroy_shader_module(fragment, None);
            Ok(check("vkCreateGraphicsPipelines", pipelines.map_err(|(_, code)| code))?[0])
        }
    }

    /// Makes a swapchain for the window as it is now, replacing the previous one.
    unsafe fn rebuild_target(&mut self) -> Result<()> {
        unsafe {
            check("vkDeviceWaitIdle", self.device.device_wait_idle())?;
            let capabilities = check("vkGetPhysicalDeviceSurfaceCapabilitiesKHR", self.base.surfaces.get_physical_device_surface_capabilities(self.physical, self.base.surface))?;
            let extent = capabilities.current_extent;
            if extent.width == 0 || extent.height == 0 || extent.width == u32::MAX {
                bail!("the window has no size Vulkan can draw at");
            }
            // The window must stay see-through, and it is drawn the way the display is held.
            let alphas = [vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED, vk::CompositeAlphaFlagsKHR::INHERIT];
            let alpha = alphas.into_iter().find(|&alpha| capabilities.supported_composite_alpha.contains(alpha)).ok_or_else(|| err!("Vulkan can only draw this window opaque"))?;
            let upright = capabilities.supported_transforms.contains(vk::SurfaceTransformFlagsKHR::IDENTITY);
            let transform = if upright { vk::SurfaceTransformFlagsKHR::IDENTITY } else { capabilities.current_transform };
            let most = if capabilities.max_image_count == 0 { u32::MAX } else { capabilities.max_image_count };
            let previous = std::mem::take(&mut self.target);
            let info = vk::SwapchainCreateInfoKHR::default()
                .surface(self.base.surface)
                .min_image_count((capabilities.min_image_count + 1).min(most))
                .image_format(self.format.format)
                .image_color_space(self.format.color_space)
                .image_extent(extent)
                .image_array_layers(1)
                .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
                .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
                .pre_transform(transform)
                .composite_alpha(alpha)
                .present_mode(vk::PresentModeKHR::FIFO)
                .clipped(true)
                .old_swapchain(previous.swapchain);
            let created = self.swapchains.create_swapchain(&info, None);
            self.discard(previous);
            self.target = Target { swapchain: check("vkCreateSwapchainKHR", created)?, extent, ..Default::default() };

            for image in check("vkGetSwapchainImagesKHR", self.swapchains.get_swapchain_images(self.target.swapchain))? {
                let view = self.create_view(image)?;
                self.target.views.push(view);
                let attachments = [view];
                let info = vk::FramebufferCreateInfo::default().render_pass(self.render_pass).attachments(&attachments).width(extent.width).height(extent.height).layers(1);
                let framebuffer = check("vkCreateFramebuffer", self.device.create_framebuffer(&info, None))?;
                self.target.framebuffers.push(framebuffer);
            }
            Ok(())
        }
    }

    unsafe fn discard(&self, target: Target) {
        unsafe {
            target.framebuffers.iter().for_each(|&framebuffer| self.device.destroy_framebuffer(framebuffer, None));
            target.views.iter().for_each(|&view| self.device.destroy_image_view(view, None));
            self.swapchains.destroy_swapchain(target.swapchain, None);
        }
    }

    unsafe fn create_view(&self, image: vk::Image) -> Result<vk::ImageView> {
        let range = vk::ImageSubresourceRange::default().aspect_mask(vk::ImageAspectFlags::COLOR).level_count(1).layer_count(1);
        let info = vk::ImageViewCreateInfo::default().image(image).view_type(vk::ImageViewType::TYPE_2D).format(self.format.format).subresource_range(range);
        unsafe { check("vkCreateImageView", self.device.create_image_view(&info, None)) }
    }

    unsafe fn allocate(&self, needs: vk::MemoryRequirements, flags: vk::MemoryPropertyFlags) -> Result<vk::DeviceMemory> {
        let fits = |index: &u32| needs.memory_type_bits & (1 << index) != 0 && self.memory.memory_types[*index as usize].property_flags.contains(flags);
        let kind = (0..self.memory.memory_type_count).find(fits).ok_or_else(|| err!("the Vulkan device has no suitable memory"))?;
        unsafe { check("vkAllocateMemory", self.device.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(needs.size).memory_type_index(kind), None)) }
    }

    /// A buffer the CPU can write to, at least `size` bytes long.
    unsafe fn create_buffer(&self, usage: vk::BufferUsageFlags, size: u64) -> Result<Buffer> {
        unsafe {
            let mut made = Buffer::empty(usage);
            made.capacity = size.next_power_of_two().max(SMALLEST_BUFFER);
            let info = vk::BufferCreateInfo::default().size(made.capacity).usage(usage).sharing_mode(vk::SharingMode::EXCLUSIVE);
            made.buffer = check("vkCreateBuffer", self.device.create_buffer(&info, None))?;
            let visible = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
            let ready: Result<()> = (|| {
                made.memory = self.allocate(self.device.get_buffer_memory_requirements(made.buffer), visible)?;
                check("vkBindBufferMemory", self.device.bind_buffer_memory(made.buffer, made.memory, 0))?;
                made.mapped = check("vkMapMemory", self.device.map_memory(made.memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()))?.cast();
                Ok(())
            })();
            ready.inspect_err(|_| self.destroy_buffer(&made)).map(|()| made)
        }
    }

    unsafe fn destroy_buffer(&self, buffer: &Buffer) {
        unsafe {
            self.device.destroy_buffer(buffer.buffer, None);
            self.device.free_memory(buffer.memory, None);
        }
    }

    /// Copies mesh data into a buffer, growing it first when it is too small.
    unsafe fn fill<'a>(&self, buffer: &mut Buffer, size: u64, parts: impl Iterator<Item = &'a [u8]>) -> Result<()> {
        unsafe {
            if buffer.capacity < size.max(1) {
                let larger = self.create_buffer(buffer.usage, size)?;
                self.destroy_buffer(&std::mem::replace(buffer, larger));
            }
            let mut offset = 0;
            for part in parts {
                std::ptr::copy_nonoverlapping(part.as_ptr(), buffer.mapped.add(offset), part.len());
                offset += part.len();
            }
            Ok(())
        }
    }

    unsafe fn create_texture(&self, [width, height]: [u32; 2], options: TextureOptions) -> Result<Texture> {
        unsafe {
            let device = &self.device;
            let mut texture =
                Texture { image: vk::Image::null(), memory: vk::DeviceMemory::null(), view: vk::ImageView::null(), sampler: vk::Sampler::null(), set: vk::DescriptorSet::null() };
            let ready: Result<()> = (|| {
                let info = vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(vk::Format::R8G8B8A8_UNORM)
                    .extent(vk::Extent3D { width, height, depth: 1 })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST);
                texture.image = check("vkCreateImage", device.create_image(&info, None))?;
                texture.memory = self.allocate(device.get_image_memory_requirements(texture.image), vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
                check("vkBindImageMemory", device.bind_image_memory(texture.image, texture.memory, 0))?;
                let range = vk::ImageSubresourceRange::default().aspect_mask(vk::ImageAspectFlags::COLOR).level_count(1).layer_count(1);
                let view =
                    vk::ImageViewCreateInfo::default().image(texture.image).view_type(vk::ImageViewType::TYPE_2D).format(vk::Format::R8G8B8A8_UNORM).subresource_range(range);
                texture.view = check("vkCreateImageView", device.create_image_view(&view, None))?;

                let filter = |filter| if filter == TextureFilter::Linear { vk::Filter::LINEAR } else { vk::Filter::NEAREST };
                let wrap = match options.wrap_mode {
                    TextureWrapMode::ClampToEdge => vk::SamplerAddressMode::CLAMP_TO_EDGE,
                    TextureWrapMode::Repeat => vk::SamplerAddressMode::REPEAT,
                    TextureWrapMode::MirroredRepeat => vk::SamplerAddressMode::MIRRORED_REPEAT,
                };
                let sampler = vk::SamplerCreateInfo::default()
                    .mag_filter(filter(options.magnification))
                    .min_filter(filter(options.minification))
                    .address_mode_u(wrap)
                    .address_mode_v(wrap)
                    .address_mode_w(wrap);
                texture.sampler = check("vkCreateSampler", device.create_sampler(&sampler, None))?;

                let layouts = [self.set_layout];
                let sets = vk::DescriptorSetAllocateInfo::default().descriptor_pool(self.descriptors).set_layouts(&layouts);
                texture.set = check("vkAllocateDescriptorSets", device.allocate_descriptor_sets(&sets))?[0];
                let images = [vk::DescriptorImageInfo::default().sampler(texture.sampler).image_view(texture.view).image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
                let write = vk::WriteDescriptorSet::default().dst_set(texture.set).descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER).image_info(&images);
                device.update_descriptor_sets(&[write], &[]);
                Ok(())
            })();
            ready.inspect_err(|_| self.destroy_texture(&texture)).map(|()| texture)
        }
    }

    unsafe fn destroy_texture(&self, texture: &Texture) {
        unsafe {
            if texture.set != vk::DescriptorSet::null() {
                let _ = self.device.free_descriptor_sets(self.descriptors, &[texture.set]);
            }
            self.device.destroy_sampler(texture.sampler, None);
            self.device.destroy_image_view(texture.view, None);
            self.device.destroy_image(texture.image, None);
            self.device.free_memory(texture.memory, None);
        }
    }

    /// Applies one texture change. A whole image replaces the texture; a patch is copied into it.
    unsafe fn set_texture(&mut self, id: TextureId, delta: &ImageDelta) -> Result<()> {
        unsafe {
            let ImageData::Color(image) = &delta.image;
            let size = image.size.map(|side| side as u32);
            if size.contains(&0) {
                return Ok(());
            }
            if delta.pos.is_none() {
                let texture = self.create_texture(size, delta.options)?;
                if let Some(old) = self.textures.insert(id, texture) {
                    self.destroy_texture(&old);
                }
            }
            let Some(texture) = self.textures.get(&id) else { return Ok(()) };
            let pixels: &[u8] = bytemuck::cast_slice(&image.pixels);
            let staging = self.create_buffer(vk::BufferUsageFlags::TRANSFER_SRC, pixels.len() as u64)?;
            std::ptr::copy_nonoverlapping(pixels.as_ptr(), staging.mapped, pixels.len());
            let copied = self.copy_to_image(staging.buffer, texture.image, delta.pos.map_or([0; 2], |position| position.map(|side| side as i32)), size, delta.pos.is_none());
            self.destroy_buffer(&staging);
            copied
        }
    }

    unsafe fn copy_to_image(&self, source: vk::Buffer, image: vk::Image, [x, y]: [i32; 2], [width, height]: [u32; 2], fresh: bool) -> Result<()> {
        unsafe {
            let (device, commands) = (&self.device, self.commands);
            let range = vk::ImageSubresourceRange::default().aspect_mask(vk::ImageAspectFlags::COLOR).level_count(1).layer_count(1);
            let barrier = |from, to, reads, writes| {
                vk::ImageMemoryBarrier::default()
                    .old_layout(from)
                    .new_layout(to)
                    .src_access_mask(reads)
                    .dst_access_mask(writes)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(image)
                    .subresource_range(range)
            };
            let shown = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
            let before = if fresh { vk::ImageLayout::UNDEFINED } else { shown };

            check("vkBeginCommandBuffer", device.begin_command_buffer(commands, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT)))?;
            let prepare = [barrier(before, vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::AccessFlags::empty(), vk::AccessFlags::TRANSFER_WRITE)];
            device.cmd_pipeline_barrier(commands, vk::PipelineStageFlags::TOP_OF_PIPE, vk::PipelineStageFlags::TRANSFER, vk::DependencyFlags::empty(), &[], &[], &prepare);
            let layers = vk::ImageSubresourceLayers::default().aspect_mask(vk::ImageAspectFlags::COLOR).layer_count(1);
            let region =
                [vk::BufferImageCopy::default().image_subresource(layers).image_offset(vk::Offset3D { x, y, z: 0 }).image_extent(vk::Extent3D { width, height, depth: 1 })];
            device.cmd_copy_buffer_to_image(commands, source, image, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &region);
            let finish = [barrier(vk::ImageLayout::TRANSFER_DST_OPTIMAL, shown, vk::AccessFlags::TRANSFER_WRITE, vk::AccessFlags::SHADER_READ)];
            device.cmd_pipeline_barrier(commands, vk::PipelineStageFlags::TRANSFER, vk::PipelineStageFlags::FRAGMENT_SHADER, vk::DependencyFlags::empty(), &[], &[], &finish);
            check("vkEndCommandBuffer", device.end_command_buffer(commands))?;

            let buffers = [commands];
            check("vkQueueSubmit", device.queue_submit(self.queue, &[vk::SubmitInfo::default().command_buffers(&buffers)], vk::Fence::null()))?;
            check("vkQueueWaitIdle", device.queue_wait_idle(self.queue))
        }
    }

    unsafe fn draw(&mut self, frame: &Frame) -> Result<()> {
        unsafe {
            let meshes = || frame.primitives.iter().filter_map(|clipped| if let Primitive::Mesh(mesh) = &clipped.primitive { Some((clipped.clip_rect, mesh)) } else { None });
            let vertex_bytes: usize = meshes().map(|(_, mesh)| size_of_val(mesh.vertices.as_slice())).sum();
            let index_bytes: usize = meshes().map(|(_, mesh)| size_of_val(mesh.indices.as_slice())).sum();
            let (mut vertices, mut indices) = (
                std::mem::replace(&mut self.vertices, Buffer::empty(vk::BufferUsageFlags::VERTEX_BUFFER)),
                std::mem::replace(&mut self.indices, Buffer::empty(vk::BufferUsageFlags::INDEX_BUFFER)),
            );
            let filled = self.fill(&mut vertices, vertex_bytes as u64, meshes().map(|(_, mesh)| bytemuck::cast_slice(&mesh.vertices)));
            let filled = filled.and_then(|()| self.fill(&mut indices, index_bytes as u64, meshes().map(|(_, mesh)| bytemuck::cast_slice(&mesh.indices))));
            (self.vertices, self.indices) = (vertices, indices);
            filled?;

            let image = loop {
                match self.swapchains.acquire_next_image(self.target.swapchain, u64::MAX, self.acquired, vk::Fence::null()) {
                    Ok((image, _)) => break image,
                    Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => self.rebuild_target()?,
                    Err(code) => bail!("vkAcquireNextImageKHR failed: {code}"),
                }
            };

            let (device, commands, extent) = (&self.device, self.commands, self.target.extent);
            check("vkBeginCommandBuffer", device.begin_command_buffer(commands, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT)))?;
            let clear = [vk::ClearValue::default()];
            let area = vk::Rect2D { offset: vk::Offset2D::default(), extent };
            let pass =
                vk::RenderPassBeginInfo::default().render_pass(self.render_pass).framebuffer(self.target.framebuffers[image as usize]).render_area(area).clear_values(&clear);
            device.cmd_begin_render_pass(commands, &pass, vk::SubpassContents::INLINE);
            device.cmd_bind_pipeline(commands, vk::PipelineBindPoint::GRAPHICS, self.pipeline);
            device.cmd_set_viewport(commands, 0, &[vk::Viewport { x: 0.0, y: 0.0, width: extent.width as f32, height: extent.height as f32, min_depth: 0.0, max_depth: 1.0 }]);
            let points = [extent.width as f32 / frame.pixels_per_point, extent.height as f32 / frame.pixels_per_point];
            device.cmd_push_constants(commands, self.layout, vk::ShaderStageFlags::VERTEX, 0, bytemuck::cast_slice(&points));
            device.cmd_bind_vertex_buffers(commands, 0, &[self.vertices.buffer], &[0]);
            device.cmd_bind_index_buffer(commands, self.indices.buffer, 0, vk::IndexType::UINT32);

            let (mut first_vertex, mut first_index) = (0i32, 0u32);
            for (clip, mesh) in meshes() {
                let pixel = |value: f32, most: u32| ((value * frame.pixels_per_point).round().max(0.0) as u32).min(most);
                let (left, top) = (pixel(clip.min.x, extent.width), pixel(clip.min.y, extent.height));
                let (right, bottom) = (pixel(clip.max.x, extent.width), pixel(clip.max.y, extent.height));
                if let Some(texture) = self.textures.get(&mesh.texture_id).filter(|_| right > left && bottom > top) {
                    let scissor = vk::Rect2D { offset: vk::Offset2D { x: left as i32, y: top as i32 }, extent: vk::Extent2D { width: right - left, height: bottom - top } };
                    device.cmd_set_scissor(commands, 0, &[scissor]);
                    device.cmd_bind_descriptor_sets(commands, vk::PipelineBindPoint::GRAPHICS, self.layout, 0, &[texture.set], &[]);
                    device.cmd_draw_indexed(commands, mesh.indices.len() as u32, 1, first_index, first_vertex, 0);
                }
                first_vertex += mesh.vertices.len() as i32;
                first_index += mesh.indices.len() as u32;
            }
            device.cmd_end_render_pass(commands);
            check("vkEndCommandBuffer", device.end_command_buffer(commands))?;

            let (waits, stages, buffers, signals, swapchains, images) =
                ([self.acquired], [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT], [commands], [self.drawn], [self.target.swapchain], [image]);
            check("vkResetFences", device.reset_fences(&[self.idle]))?;
            let submit = vk::SubmitInfo::default().wait_semaphores(&waits).wait_dst_stage_mask(&stages).command_buffers(&buffers).signal_semaphores(&signals);
            check("vkQueueSubmit", device.queue_submit(self.queue, &[submit], self.idle))?;
            let present = vk::PresentInfoKHR::default().wait_semaphores(&signals).swapchains(&swapchains).image_indices(&images);
            match self.swapchains.queue_present(self.queue, &present) {
                Ok(_) => Ok(()),
                Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => self.rebuild_target(),
                Err(code) => bail!("vkQueuePresentKHR failed: {code}"),
            }
        }
    }
}

impl Buffer {
    fn empty(usage: vk::BufferUsageFlags) -> Self {
        Self { usage, buffer: vk::Buffer::null(), memory: vk::DeviceMemory::null(), capacity: 0, mapped: std::ptr::null_mut() }
    }
}

impl Graphics for Vulkan {
    fn name(&self) -> &'static str {
        "Vulkan"
    }

    fn max_texture_side(&self) -> usize {
        self.max_texture_side
    }

    fn rebind(&mut self, window: &NativeWindow) -> Result<()> {
        unsafe {
            check("vkDeviceWaitIdle", self.device.device_wait_idle())?;
            if window.as_ptr() != self.base.window {
                let previous = std::mem::take(&mut self.target);
                self.discard(previous);
                self.base.attach(window.as_ptr())?;
                if !check("vkGetPhysicalDeviceSurfaceSupportKHR", self.base.surfaces.get_physical_device_surface_support(self.physical, self.family, self.base.surface))? {
                    bail!("the Vulkan device cannot draw to the new window");
                }
            }
            self.rebuild_target()
        }
    }

    fn render(&mut self, frame: Frame) -> Result<()> {
        unsafe {
            check("vkWaitForFences", self.device.wait_for_fences(&[self.idle], true, u64::MAX))?;
            for (id, deltas) in frame.textures.set.drain() {
                for delta in &deltas {
                    self.set_texture(id, delta)?;
                }
            }
            self.draw(&frame)?;
            if !frame.textures.free.is_empty() {
                check("vkWaitForFences", self.device.wait_for_fences(&[self.idle], true, u64::MAX))?;
                for id in frame.textures.free.drain() {
                    if let Some(texture) = self.textures.remove(&id) {
                        self.destroy_texture(&texture);
                    }
                }
            }
            Ok(())
        }
    }
}

impl Drop for Vulkan {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            let textures = std::mem::take(&mut self.textures);
            textures.values().for_each(|texture| self.destroy_texture(texture));
            self.destroy_buffer(&self.vertices);
            self.destroy_buffer(&self.indices);
            let target = std::mem::take(&mut self.target);
            self.discard(target);
            self.device.destroy_pipeline(self.pipeline, None);
            self.device.destroy_render_pass(self.render_pass, None);
            self.device.destroy_descriptor_pool(self.descriptors, None);
            self.device.destroy_pipeline_layout(self.layout, None);
            self.device.destroy_descriptor_set_layout(self.set_layout, None);
            self.device.destroy_fence(self.idle, None);
            self.device.destroy_semaphore(self.drawn, None);
            self.device.destroy_semaphore(self.acquired, None);
            self.device.destroy_command_pool(self.pool, None);
            self.device.destroy_device(None);
        }
    }
}
