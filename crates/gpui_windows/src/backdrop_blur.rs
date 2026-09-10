//! Backdrop blur for the DirectX backend — the pass a pane of glass needs.
//!
//! Unlike [`crate::black_hole_post_process`], which distorts the finished
//! window as one image, this runs *inside* the scene: it snapshots what has
//! been drawn so far, blurs that snapshot within one rounded rectangle, and
//! writes it back, so whatever is painted afterwards composites on top. That
//! ordering is the whole point — it is what makes a panel blur what is behind
//! it instead of blurring itself.
//!
//! The gaussian is a single pass with a fixed kernel rather than the usual
//! separable pair, which would need a scratch target and two draws per panel.
//! At the radii a glass panel asks for, one pass is cheaper and the difference
//! is not visible through a translucent tint.

use std::slice;

use anyhow::{Context, Result};
use gpui::BackdropBlur;
use windows::Win32::Graphics::{
    Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
    Direct3D11::*,
    Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
};

use crate::directx_renderer::shader_resources::{RawShaderBytes, ShaderModule, ShaderTarget};

/// How far outside its own bounds the blur reaches for samples. Beyond two
/// sigma a gaussian's weights fall under the precision of an 8-bit target, so
/// this is the whole neighbourhood the snapshot has to carry.
fn sample_reach(sigma: f32) -> f32 {
    (sigma * 2.0).ceil().max(1.0)
}

/// DirectX resources for the backdrop blur pass.
pub(super) struct BackdropBlurRenderer {
    vertex: ID3D11VertexShader,
    fragment: ID3D11PixelShader,
    params_buffer: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    blend_state: ID3D11BlendState,
    input_texture: Option<ID3D11Texture2D>,
    input_srv: Option<ID3D11ShaderResourceView>,
}

impl BackdropBlurRenderer {
    pub(super) fn new(device: &ID3D11Device) -> Result<Self> {
        let vertex = create_vertex_shader(
            device,
            RawShaderBytes::new(ShaderModule::BackdropBlur, ShaderTarget::Vertex)?.as_bytes(),
        )?;
        let fragment = create_fragment_shader(
            device,
            RawShaderBytes::new(ShaderModule::BackdropBlur, ShaderTarget::Fragment)?.as_bytes(),
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

    /// Discards the size-dependent snapshot after the swap chain is resized.
    pub(super) fn reset_input(&mut self) {
        self.input_texture = None;
        self.input_srv = None;
    }

    /// Snapshot what has been drawn so far and write a blurred copy of it back
    /// inside `blur`'s rounded rect.
    ///
    /// Called between primitive batches, so the snapshot holds exactly the
    /// layers beneath the panel. The render target has to be unbound for the
    /// copy: it cannot be an output and a shader input at the same time.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn draw(
        &mut self,
        device: &ID3D11Device,
        device_context: &ID3D11DeviceContext,
        render_target: &ID3D11Texture2D,
        render_target_view: &Option<ID3D11RenderTargetView>,
        viewport: &D3D11_VIEWPORT,
        width: u32,
        height: u32,
        blur: &BackdropBlur,
    ) -> Result<()> {
        let origin = [blur.bounds.origin.x.0, blur.bounds.origin.y.0];
        let size = [blur.bounds.size.width.0, blur.bounds.size.height.0];
        if size[0] <= 0.0 || size[1] <= 0.0 {
            return Ok(());
        }
        let sigma = blur.sigma.0.max(1e-4);

        let (input_texture, input_srv) = self
            .input(device, width, height)
            .context("Preparing backdrop blur input")?;

        // Only the neighbourhood the kernel can reach, not the whole window: a
        // window full of glass panels would otherwise copy the entire target
        // once per panel, every frame.
        let reach = sample_reach(sigma);
        let region = D3D11_BOX {
            left: (origin[0] - reach).max(0.0) as u32,
            top: (origin[1] - reach).max(0.0) as u32,
            right: ((origin[0] + size[0] + reach).min(width as f32) as u32).max(1),
            bottom: ((origin[1] + size[1] + reach).min(height as f32) as u32).max(1),
            front: 0,
            back: 1,
        };
        if region.right <= region.left || region.bottom <= region.top {
            return Ok(());
        }

        unsafe {
            device_context.OMSetRenderTargets(None, None);
            device_context.CopySubresourceRegion(
                &input_texture,
                0,
                region.left,
                region.top,
                0,
                render_target,
                0,
                Some(&region),
            );
            device_context.OMSetRenderTargets(Some(slice::from_ref(render_target_view)), None);
        }

        // A radius past half the shorter side makes the SDF read every fragment
        // as outside and the whole rect discards. `paint_backdrop_blur` already
        // clamps, but the renderer does not get to assume its caller did.
        let limit = size[0].min(size[1]) * 0.5;
        update_params(
            device_context,
            &self.params_buffer,
            &BackdropBlurParams {
                origin,
                size,
                corner_radius: blur.corner_radii.top_left.0.clamp(0.0, limit),
                sigma,
                viewport_size: [viewport.Width, viewport.Height],
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
            // Both stages, and slot 2: this shader reads the rect out of the
            // cbuffer in the vertex stage as well, and slots 0 and 1 belong to
            // gpui's own globals, which it binds once and never rebinds.
            device_context.VSSetConstantBuffers(2, Some(&constant_buffers));
            device_context.PSSetConstantBuffers(2, Some(&constant_buffers));
            device_context.PSSetSamplers(0, Some(&samplers));
            device_context.PSSetShaderResources(0, Some(&shader_resources));
            device_context.OMSetBlendState(&self.blend_state, None, 0xFFFFFFFF);
            device_context.Draw(4, 0);
            // Unbind, or the next batch cannot use this texture as an output.
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
                .context("Missing backdrop blur input texture")?,
            self.input_srv
                .clone()
                .context("Missing backdrop blur input shader resource view")?,
        ))
    }
}

/// Mirrors the `BackdropBlurParams` cbuffer in `backdrop_blur.hlsl`.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
struct BackdropBlurParams {
    origin: [f32; 2],
    size: [f32; 2],
    corner_radius: f32,
    sigma: f32,
    viewport_size: [f32; 2],
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
        output.context("Creating backdrop blur input texture")?
    };

    let mut view = None;
    unsafe { device.CreateShaderResourceView(&texture, None, Some(&mut view))? };

    Ok((
        texture,
        view.context("Creating backdrop blur input shader resource view")?,
    ))
}

fn create_vertex_shader(device: &ID3D11Device, bytes: &[u8]) -> Result<ID3D11VertexShader> {
    unsafe {
        let mut shader = None;
        device.CreateVertexShader(bytes, None, Some(&mut shader))?;
        shader.context("Creating backdrop blur vertex shader")
    }
}

fn create_fragment_shader(device: &ID3D11Device, bytes: &[u8]) -> Result<ID3D11PixelShader> {
    unsafe {
        let mut shader = None;
        device.CreatePixelShader(bytes, None, Some(&mut shader))?;
        shader.context("Creating backdrop blur fragment shader")
    }
}

fn create_constant_buffer(device: &ID3D11Device) -> Result<ID3D11Buffer> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: std::mem::size_of::<BackdropBlurParams>() as u32,
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        ..Default::default()
    };
    let mut buffer = None;
    unsafe { device.CreateBuffer(&desc, None, Some(&mut buffer))? };
    buffer.context("Creating backdrop blur constant buffer")
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
        output.context("Creating backdrop blur sampler")
    }
}

fn create_blend_state(device: &ID3D11Device) -> Result<ID3D11BlendState> {
    let mut desc = D3D11_BLEND_DESC::default();
    // The shader premultiplies its own coverage, so the edge feather blends
    // against what is already there while the interior overwrites it.
    desc.RenderTarget[0].BlendEnable = true.into();
    desc.RenderTarget[0].SrcBlend = D3D11_BLEND_ONE;
    desc.RenderTarget[0].DestBlend = D3D11_BLEND_INV_SRC_ALPHA;
    desc.RenderTarget[0].BlendOp = D3D11_BLEND_OP_ADD;
    desc.RenderTarget[0].SrcBlendAlpha = D3D11_BLEND_ONE;
    desc.RenderTarget[0].DestBlendAlpha = D3D11_BLEND_INV_SRC_ALPHA;
    desc.RenderTarget[0].BlendOpAlpha = D3D11_BLEND_OP_ADD;
    desc.RenderTarget[0].RenderTargetWriteMask = D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8;
    unsafe {
        let mut state = None;
        device.CreateBlendState(&desc, Some(&mut state))?;
        state.context("Creating backdrop blur blend state")
    }
}

fn update_params(
    device_context: &ID3D11DeviceContext,
    buffer: &ID3D11Buffer,
    params: &BackdropBlurParams,
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
    use super::BackdropBlurParams;

    /// The cbuffer is `float2 + float2 + float + float + float2` = 8 floats,
    /// which fills two 16-byte registers exactly, so HLSL adds no padding the
    /// Rust side would have to mirror.
    #[test]
    fn backdrop_blur_params_layout_matches_the_cbuffer() {
        assert_eq!(std::mem::size_of::<BackdropBlurParams>(), 8 * 4);
        assert_eq!(std::mem::size_of::<BackdropBlurParams>() % 16, 0);
    }
}
