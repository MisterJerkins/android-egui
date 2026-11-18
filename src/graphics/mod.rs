#[cfg(target_os = "android")]
mod opengl;
mod software;
#[cfg(target_os = "android")]
#[allow(unsafe_code)]
mod vulkan;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Backend {
    #[default]
    Auto,
    Software,
    OpenGl,
    Vulkan,
}

impl Backend {
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "software" | "cpu" => Some(Self::Software),
            "opengl" | "gl" | "gles" => Some(Self::OpenGl),
            "vulkan" | "vk" => Some(Self::Vulkan),
            _ => None,
        }
    }

    /// The backends to try, best first.
    pub(crate) fn candidates(self) -> &'static [Self] {
        match self {
            Self::Auto => &[Self::OpenGl, Self::Vulkan, Self::Software],
            Self::Software => &[Self::Software],
            Self::OpenGl => &[Self::OpenGl],
            Self::Vulkan => &[Self::Vulkan],
        }
    }
}

#[cfg(target_os = "android")]
pub(crate) use android::{Frame, Graphics, create};

#[cfg(target_os = "android")]
mod android {
    use egui::{ClippedPrimitive, TexturesDelta};

    use super::{Backend, opengl, software, vulkan};
    use crate::Result;
    use crate::window::NativeWindow;

    pub struct Frame<'a> {
        pub window: &'a NativeWindow,
        pub size: [u32; 2],
        pub pixels_per_point: f32,
        pub primitives: &'a [ClippedPrimitive],
        pub textures: &'a mut TexturesDelta,
    }

    pub trait Graphics {
        fn name(&self) -> &'static str;
        fn max_texture_side(&self) -> usize;
        fn rebind(&mut self, window: &NativeWindow) -> Result<()>;
        fn render(&mut self, frame: Frame) -> Result<()>;
    }

    pub fn create(backend: Backend, window: &NativeWindow) -> Result<Box<dyn Graphics>> {
        match backend {
            Backend::Auto | Backend::OpenGl => Ok(Box::new(opengl::OpenGl::new(window)?)),
            Backend::Software => Ok(Box::new(software::Software::new())),
            Backend::Vulkan => Ok(Box::new(vulkan::Vulkan::new(window)?)),
        }
    }
}
