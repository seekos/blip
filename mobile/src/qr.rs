use std::sync::Arc;

use gpui::{Image, ImageFormat};
use qrcode::{QrCode, render::svg};

const MIN_SIZE: u32 = 168;

pub(crate) fn qr_code_image(value: &str) -> Result<(Arc<Image>, f32), String> {
    let code = QrCode::new(value.as_bytes()).map_err(|error| error.to_string())?;
    let modules = code.width() as u32 + 8;
    let module_size = MIN_SIZE.div_ceil(modules);
    let size = modules * module_size;
    let svg = code
        .render::<svg::Color>()
        .module_dimensions(module_size, module_size)
        .build();
    Ok((
        Arc::new(Image::from_bytes(ImageFormat::Svg, svg.into_bytes())),
        size as f32,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_url_becomes_a_square_scannable_svg() {
        let (image, size) = qr_code_image("http://192.168.1.10:1986").unwrap();
        let svg = std::str::from_utf8(&image.bytes).unwrap();

        assert_eq!(image.format, ImageFormat::Svg);
        assert!(size >= MIN_SIZE as f32);
        assert!(svg.contains(&format!(r#"width="{}""#, size as u32)));
        assert!(svg.contains(&format!(r#"height="{}""#, size as u32)));
        assert!(svg.contains("shape-rendering=\"crispEdges\""));
        assert!(svg.contains("fill=\"#000\""));
        assert!(svg.contains("fill=\"#fff\""));
    }
}
