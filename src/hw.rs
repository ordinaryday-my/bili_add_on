use std::ptr;

use anyhow::{Result, anyhow};
use ffmpeg::ffi;
use ffmpeg_next as ffmpeg;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwCodec {
    Nvenc,
    Amf,
    Qsv,
}

impl HwCodec {
    pub fn encoder_name(self) -> &'static str {
        match self {
            Self::Nvenc => "h264_nvenc",
            Self::Amf => "h264_amf",
            Self::Qsv => "h264_qsv",
        }
    }

    pub fn device_type(self) -> ffi::AVHWDeviceType {
        match self {
            Self::Nvenc => ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
            Self::Amf => ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
            Self::Qsv => ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_QSV,
        }
    }

    pub fn hw_pixel_format(self) -> ffi::AVPixelFormat {
        match self {
            Self::Nvenc => ffi::AVPixelFormat::AV_PIX_FMT_CUDA,
            Self::Amf => ffi::AVPixelFormat::AV_PIX_FMT_D3D11,
            Self::Qsv => ffi::AVPixelFormat::AV_PIX_FMT_QSV,
        }
    }

    pub fn hw_pixel_rust(self) -> ffmpeg::util::format::Pixel {
        match self {
            Self::Nvenc => ffmpeg::util::format::Pixel::CUDA,
            Self::Amf => ffmpeg::util::format::Pixel::D3D11,
            Self::Qsv => ffmpeg::util::format::Pixel::QSV,
        }
    }

    pub fn all() -> [Self; 3] {
        [Self::Nvenc, Self::Amf, Self::Qsv]
    }

    pub fn from_cli(s: &str) -> Option<Self> {
        match s {
            "nvenc" => Some(Self::Nvenc),
            "amf" => Some(Self::Amf),
            "qsv" => Some(Self::Qsv),
            _ => None,
        }
    }
}

pub struct HwSetup {
    pub frames_ref: *mut ffi::AVBufferRef,
}

impl Drop for HwSetup {
    fn drop(&mut self) {
        if !self.frames_ref.is_null() {
            unsafe {
                ffi::av_buffer_unref(&mut self.frames_ref);
            }
        }
    }
}

unsafe impl Send for HwSetup {}

pub unsafe fn try_create_hardware_setup(
    hw_codec: HwCodec,
    width: i32,
    height: i32,
) -> Result<HwSetup> {
    let device_type = hw_codec.device_type();
    let mut device_ref: *mut ffi::AVBufferRef = ptr::null_mut();

    let ret = unsafe {
        ffi::av_hwdevice_ctx_create(
            &mut device_ref,
            device_type,
            ptr::null(),
            ptr::null_mut(),
            0,
        )
    };
    if ret < 0 {
        return Err(anyhow!(
            "创建 {} 硬件设备上下文失败: 错误码 {}",
            hw_codec.encoder_name(),
            ret
        ));
    }

    let mut frames_ref = unsafe { ffi::av_hwframe_ctx_alloc(device_ref) };
    unsafe { ffi::av_buffer_unref(&mut device_ref) };

    if frames_ref.is_null() {
        return Err(anyhow!("分配硬件帧上下文失败"));
    }

    let hw_pix_fmt = hw_codec.hw_pixel_format();
    unsafe {
        let ctx = (*frames_ref).data as *mut ffi::AVHWFramesContext;
        (*ctx).format = hw_pix_fmt;
        (*ctx).sw_format = ffi::AVPixelFormat::AV_PIX_FMT_NV12;
        (*ctx).width = width;
        (*ctx).height = height;
        (*ctx).initial_pool_size = 16;
    };

    let ret = unsafe { ffi::av_hwframe_ctx_init(frames_ref) };
    if ret < 0 {
        unsafe { ffi::av_buffer_unref(&mut frames_ref) };
        return Err(anyhow!(
            "初始化 {} 硬件帧上下文失败: 错误码 {}",
            hw_codec.encoder_name(),
            ret
        ));
    }

    Ok(HwSetup { frames_ref })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hw_codec_encoder_names() {
        assert_eq!(HwCodec::Nvenc.encoder_name(), "h264_nvenc");
        assert_eq!(HwCodec::Amf.encoder_name(), "h264_amf");
        assert_eq!(HwCodec::Qsv.encoder_name(), "h264_qsv");
    }

    #[test]
    fn test_hw_codec_from_cli_valid() {
        assert_eq!(HwCodec::from_cli("nvenc"), Some(HwCodec::Nvenc));
        assert_eq!(HwCodec::from_cli("amf"), Some(HwCodec::Amf));
        assert_eq!(HwCodec::from_cli("qsv"), Some(HwCodec::Qsv));
    }

    #[test]
    fn test_hw_codec_from_cli_invalid() {
        assert_eq!(HwCodec::from_cli("cuda"), None);
        assert_eq!(HwCodec::from_cli(""), None);
        assert_eq!(HwCodec::from_cli("auto"), None);
    }

    #[test]
    fn test_hw_codec_all_contains_three() {
        assert_eq!(HwCodec::all().len(), 3);
    }

    #[test]
    fn test_hw_codec_device_types() {
        assert_eq!(
            HwCodec::Nvenc.device_type(),
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA
        );
        assert_eq!(
            HwCodec::Amf.device_type(),
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA
        );
        assert_eq!(
            HwCodec::Qsv.device_type(),
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_QSV
        );
    }

    #[test]
    fn test_hw_codec_pixel_formats() {
        assert_eq!(
            HwCodec::Nvenc.hw_pixel_format(),
            ffi::AVPixelFormat::AV_PIX_FMT_CUDA
        );
        assert_eq!(
            HwCodec::Amf.hw_pixel_format(),
            ffi::AVPixelFormat::AV_PIX_FMT_D3D11
        );
        assert_eq!(
            HwCodec::Qsv.hw_pixel_format(),
            ffi::AVPixelFormat::AV_PIX_FMT_QSV
        );
    }

    #[test]
    fn test_hw_codec_pixel_rust() {
        use ffmpeg_next::util::format::Pixel;
        assert_eq!(HwCodec::Nvenc.hw_pixel_rust(), Pixel::CUDA);
        assert_eq!(HwCodec::Amf.hw_pixel_rust(), Pixel::D3D11);
        assert_eq!(HwCodec::Qsv.hw_pixel_rust(), Pixel::QSV);
    }
}
