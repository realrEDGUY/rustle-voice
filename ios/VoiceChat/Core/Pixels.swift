import CoreGraphics

/// A `CGImage` over pixels the Rust core lends (no copy). The data provider's
/// release callback calls `vc_pixels_release`, which drops the core's reference,
/// so the pixels live exactly as long as the image.
func makeCGImage(_ px: VcPixels) -> CGImage? {
    guard let handle = px.handle else { return nil }
    guard let data = px.data, px.width > 0, px.height > 0,
          px.len >= Int(px.width) * Int(px.height) * 4
    else {
        vc_pixels_release(handle)
        return nil
    }
    let release: CGDataProviderReleaseDataCallback = { info, _, _ in
        vc_pixels_release(info)
    }
    guard let provider = CGDataProvider(dataInfo: handle, data: data, size: px.len, releaseData: release) else {
        vc_pixels_release(handle)
        return nil
    }
    // From here the provider owns the handle: it releases it when it goes away,
    // also when the image below cannot be made.
    return CGImage(
        width: Int(px.width),
        height: Int(px.height),
        bitsPerComponent: 8,
        bitsPerPixel: 32,
        bytesPerRow: Int(px.width) * 4,
        space: CGColorSpace(name: CGColorSpace.sRGB) ?? CGColorSpaceCreateDeviceRGB(),
        // egui's Color32 is premultiplied RGBA.
        bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.premultipliedLast.rawValue),
        provider: provider,
        decode: nil,
        shouldInterpolate: true,
        intent: .defaultIntent
    )
}
