use anyhow::{Context, Result, bail};
use std::mem::ManuallyDrop;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::core::{BOOL, Interface};

pub struct Nv12Converter {
    context: ID3D11VideoContext1,
    processor: ID3D11VideoProcessor,
    input: ID3D11VideoProcessorInputView,
    target: ID3D11VideoProcessorOutputView,
    texture: ID3D11Texture2D,
}

impl Nv12Converter {
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        rgb: &ID3D11Texture2D,
        source: (u32, u32),
        destination: (u32, u32),
        fps: u32,
    ) -> Result<Self> {
        let video: ID3D11VideoDevice = device.cast().context("NVIDIA D3D11 视频处理不可用")?;
        let context: ID3D11VideoContext1 = context.cast()?;
        let rate = DXGI_RATIONAL {
            Numerator: fps,
            Denominator: 1,
        };
        let config = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: source.0,
            InputHeight: source.1,
            OutputFrameRate: rate,
            OutputWidth: destination.0,
            OutputHeight: destination.1,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        let modes = unsafe { video.CreateVideoProcessorEnumerator(&config) }
            .context("创建视频处理器配置")?;
        let supported_input =
            unsafe { modes.CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM) }
                .context("查询 BGRA 输入能力")?;
        let supported_output = unsafe { modes.CheckVideoProcessorFormat(DXGI_FORMAT_NV12) }
            .context("查询 NV12 输出能力")?;
        if supported_input & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32 == 0
            || supported_output & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 == 0
        {
            bail!("D3D11 视频处理器不支持 BGRA 转 NV12");
        }
        if let Ok(modes) = modes.cast::<ID3D11VideoProcessorEnumerator1>() {
            let supported = unsafe {
                modes.CheckVideoProcessorFormatConversion(
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                    DXGI_FORMAT_NV12,
                    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
                )
            }
            .context("查询显式 BT.709 转换能力")?;
            if !supported.as_bool() {
                bail!("NVIDIA 视频处理器不支持所需的 BT.709 转换");
            }
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: destination.0,
            Height: destination.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32,
            ..Default::default()
        };
        let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
            ..Default::default()
        };
        let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let dimensions = |size: (u32, u32)| RECT {
            left: 0,
            top: 0,
            right: size.0 as i32,
            bottom: size.1 as i32,
        };
        unsafe {
            let processor = video
                .CreateVideoProcessor(&modes, 0)
                .context("创建 D3D11 视频处理器")?;
            let mut texture = None;
            device
                .CreateTexture2D(&desc, None, Some(&mut texture))
                .context("创建 NV12 输出纹理")?;
            let texture = texture.context("没有取得 NV12 纹理")?;
            let (mut input, mut target) = (None, None);
            video
                .CreateVideoProcessorInputView(rgb, &modes, &input_desc, Some(&mut input))
                .context("绑定 BGRA 输入视图")?;
            video
                .CreateVideoProcessorOutputView(&texture, &modes, &output_desc, Some(&mut target))
                .context("绑定 NV12 输出视图")?;
            context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            context.VideoProcessorSetStreamColorSpace1(
                &processor,
                0,
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            );
            context.VideoProcessorSetOutputColorSpace1(
                &processor,
                DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
            );
            context.VideoProcessorSetStreamSourceRect(
                &processor,
                0,
                true,
                Some(&dimensions(source)),
            );
            context.VideoProcessorSetStreamDestRect(
                &processor,
                0,
                true,
                Some(&dimensions(destination)),
            );
            context.VideoProcessorSetOutputTargetRect(
                &processor,
                true,
                Some(&dimensions(destination)),
            );
            Ok(Self {
                context,
                processor,
                input: input.context("没有 BGRA 输入视图")?,
                target: target.context("没有 NV12 输出视图")?,
                texture,
            })
        }
    }

    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.texture
    }

    pub fn convert(&self) -> Result<()> {
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: BOOL(1),
            pInputSurface: ManuallyDrop::new(Some(self.input.clone())),
            ..Default::default()
        };
        let result = unsafe {
            self.context.VideoProcessorBlt(
                &self.processor,
                &self.target,
                0,
                std::slice::from_ref(&stream),
            )
        };
        unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };
        result.context("GPU 转换 BT.709 NV12 失败")
    }
}
