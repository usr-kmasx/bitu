//! bitu-layer: camada Vulkan + hook OpenGL para travar o frame time.
//! Sem este APP rodando, com env zerado, fica inerte (pacing desligado).

pub mod gl;
pub mod inject;
pub mod pacing;
pub mod vulkan;
