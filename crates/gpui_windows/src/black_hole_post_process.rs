use std::slice;

use anyhow::{Context, Result};
use gpui::BlackHolePostProcess;
use windows::Win32::Graphics::{
    Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
    Direct3D11::*,
    Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
};

use crate::directx_renderer::shader_resources::{RawShaderBytes, ShaderModule, ShaderTarget};

/// DirectX resources for the full-window black hole post-process pass.
pub(super) struct BlackHolePostProcessRenderer {
    vertex: ID3D11VertexShader,
    fragment: ID3D11PixelShader,
    params_buffer: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    blend_state: ID3D11BlendState,
    input_texture: Option<ID3D11Texture2D>,
    input_srv: Option<ID3D11ShaderResourceView>,
}

impl BlackHolePostProcessRenderer {
    pub(super) fn new(device: &ID3D11Device) -> Result<Self> {
        let vertex = create_vertex_shader(
            device,
            RawShaderBytes::new(ShaderModule::BlackHolePostProcess, ShaderTarget::Vertex)?
                .as_bytes(),
        )?;
        let fragment = create_fragment_shader(
            device,
            RawShaderBytes::new(ShaderModule::BlackHolePostProcess, ShaderTarget::Fragment)?
                .as_bytes(),
        )?;

        Ok(Self {
            vertex,
            fragment,
            params_buffer: create_constant_buffer(device)?,
            sampler: create_sampler(device)?,
            blend_state: create_blend_state(device)?,
            input_texture: None,
            input_srv: None,
        })
    }

    /// Discards the size-dependent input copy after the swap chain is resized.
    pub(super) fn reset_input(&mut self) {
        self.input_texture = None;
        self.input_srv = None;
    }

    /// Copies the completed window and runs the fullscreen effect back into the
    /// swap-chain render target.
    pub(super) fn draw(
        &mut self,
        device: &ID3D11Device,
        device_context: &ID3D11DeviceContext,
        render_target: &ID3D11Texture2D,
        render_target_view: &Option<ID3D11RenderTargetView>,
        viewport: &D3D11_VIEWPORT,
        width: u32,
        height: u32,
        effect: &BlackHolePostProcess,
    ) -> Result<()> {
        let (input_texture, input_srv) = self
            .input(device, width, height)
            .context("Preparing black hole post-process input")?;

        unsafe {
            device_context.OMSetRenderTargets(None, None);
            device_context.CopyResource(&input_texture, render_target);
            device_context.OMSetRenderTargets(Some(slice::from_ref(render_target_view)), None);
        }

        update_params(
            device_context,
            &self.params_buffer,
            &BlackHolePostProcessParams {
                center: [effect.center.x, effect.center.y],
                radius: effect.radius.max(1e-4),
                time: effect.time,
                intensity: effect.intensity.clamp(0.0, 1.0),
                viewport_size: [viewport.Width, viewport.Height],
                _pad: 0.0,
            },
        )?;

        let constant_buffers = [Some(self.params_buffer.clone())];
        let samplers = [Some(self.sampler.clone())];
        let shader_resources = [Some(input_srv)];
        unsafe {
            device_context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            device_context.RSSetViewports(Some(slice::from_ref(viewport)));
            device_context.VSSetShader(&self.vertex, None);
            device_context.PSSetShader(&self.fragment, None);
            device_context.PSSetConstantBuffers(0, Some(&constant_buffers));
            device_context.PSSetSamplers(0, Some(&samplers));
            device_context.PSSetShaderResources(0, Some(&shader_resources));
            device_context.OMSetBlendState(&self.blend_state, None, 0xFFFFFFFF);
            device_context.Draw(4, 0);
            device_context.PSSetShaderResources(0, Some(&[None]));
        }

        Ok(())
    }

    fn input(
        &mut self,
        device: &ID3D11Device,
        width: u32,
        height: u32,
    ) -> Result<(ID3D11Texture2D, ID3D11ShaderResourceView)> {
        if self.input_texture.is_none() {
            let (texture, view) = create_input_texture(device, width, height)?;
            self.input_texture = Some(texture);
            self.input_srv = Some(view);
        }

        Ok((
            self.input_texture
                .clone()
                .context("Missing black hole post-process input texture")?,
            self.input_srv
                .clone()
                .context("Missing black hole post-process input shader resource view")?,
        ))
    }
}

/// Mirrors the `BlackHolePostProcessParams` cbuffer in
/// `black_hole_post_process.hlsl`.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
struct BlackHolePostProcessParams {
    center: [f32; 2],
    radius: f32,
    time: f32,
    intensity: f32,
    viewport_size: [f32; 2],
    _pad: f32,
}

fn create_input_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<(ID3D11Texture2D, ID3D11ShaderResourceView)> {
    let texture = unsafe {
        let mut output = None;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width.max(1),
            Height: height.max(1),
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        device.CreateTexture2D(&desc, None, Some(&mut output))?;
        output.context("Creating black hole post-process input texture")?
    };

    let mut view = None;
    unsafe { device.CreateShaderResourceView(&texture, None, Some(&mut view))? };

    Ok((
        texture,
        view.context("Creating black hole post-process input shader resource view")?,
    ))
}

fn create_vertex_shader(device: &ID3D11Device, bytes: &[u8]) -> Result<ID3D11VertexShader> {
    unsafe {
        let mut shader = None;
        device.CreateVertexShader(bytes, None, Some(&mut shader))?;
        shader.context("Creating black hole post-process vertex shader")
    }
}

fn create_fragment_shader(device: &ID3D11Device, bytes: &[u8]) -> Result<ID3D11PixelShader> {
    unsafe {
        let mut shader = None;
        device.CreatePixelShader(bytes, None, Some(&mut shader))?;
        shader.context("Creating black hole post-process fragment shader")
    }
}

fn create_constant_buffer(device: &ID3D11Device) -> Result<ID3D11Buffer> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: std::mem::size_of::<BlackHolePostProcessParams>() as u32,
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        ..Default::default()
    };
    let mut buffer = None;
    unsafe { device.CreateBuffer(&desc, None, Some(&mut buffer))? };
    buffer.context("Creating black hole post-process constant buffer")
}

fn create_sampler(device: &ID3D11Device) -> Result<ID3D11SamplerState> {
    let desc = D3D11_SAMPLER_DESC {
        Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
        AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
        MipLODBias: 0.0,
        MaxAnisotropy: 1,
        ComparisonFunc: D3D11_COMPARISON_ALWAYS,
        BorderColor: [0.0; 4],
        MinLOD: 0.0,
        MaxLOD: D3D11_FLOAT32_MAX,
    };
    unsafe {
        let mut output = None;
        device.CreateSamplerState(&desc, Some(&mut output))?;
        output.context("Creating black hole post-process sampler")
    }
}

fn create_blend_state(device: &ID3D11Device) -> Result<ID3D11BlendState> {
    let mut desc = D3D11_BLEND_DESC::default();
    desc.RenderTarget[0].BlendEnable = false.into();
    desc.RenderTarget[0].RenderTargetWriteMask = D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8;
    unsafe {
        let mut state = None;
        device.CreateBlendState(&desc, Some(&mut state))?;
        state.context("Creating black hole post-process blend state")
    }
}

fn update_params(
    device_context: &ID3D11DeviceContext,
    buffer: &ID3D11Buffer,
    params: &BlackHolePostProcessParams,
) -> Result<()> {
    unsafe {
        let mut destination = std::mem::zeroed();
        device_context.Map(
            buffer,
            0,
            D3D11_MAP_WRITE_DISCARD,
            0,
            Some(&mut destination),
        )?;
        std::ptr::copy_nonoverlapping(params, destination.pData.cast(), 1);
        device_context.Unmap(buffer, 0);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::BlackHolePostProcessParams;
    #[cfg(debug_assertions)]
    use crate::directx_renderer::shader_resources::ShaderModule;

    #[test]
    fn test_black_hole_post_process_params_layout() {
        assert_eq!(std::mem::size_of::<BlackHolePostProcessParams>(), 32);
        assert_eq!(std::mem::size_of::<BlackHolePostProcessParams>() % 16, 0);
        assert_eq!(std::mem::offset_of!(BlackHolePostProcessParams, center), 0);
        assert_eq!(std::mem::offset_of!(BlackHolePostProcessParams, radius), 8);
        assert_eq!(std::mem::offset_of!(BlackHolePostProcessParams, time), 12);
        assert_eq!(
            std::mem::offset_of!(BlackHolePostProcessParams, intensity),
            16
        );
        assert_eq!(
            std::mem::offset_of!(BlackHolePostProcessParams, viewport_size),
            20
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    fn test_black_hole_post_process_shader_module_name() {
        assert_eq!(
            ShaderModule::BlackHolePostProcess.as_str(),
            "black_hole_post_process"
        );
    }
}
