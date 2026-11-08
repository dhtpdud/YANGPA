//! 화면에 띄울 QR 코드(기기 연결 링크, 브리지 공유).

use yangpa_rpc::api::QrCode;

use crate::error::{Error, Result};

pub(crate) fn encode(text: &str) -> Result<QrCode> {
    let code = qrcodegen::QrCode::encode_text(text, qrcodegen::QrCodeEcc::Medium)
        .map_err(|_| Error::internal("QR 코드를 만들 수 없음"))?;
    let n = code.size();
    let modules = (0..n)
        .flat_map(|y| (0..n).map(move |x| (x, y)))
        .map(|(x, y)| if code.get_module(x, y) { '1' } else { '0' })
        .collect();
    Ok(QrCode {
        size: n as u32,
        modules,
    })
}
