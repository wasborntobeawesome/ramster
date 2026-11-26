// Renders the ramster app icon (a memory stick on a rounded square) at every
// iconset size with CoreGraphics. Usage:
//   swift packaging/make-icon.swift <out.iconset> [preview-512.png]
//   iconutil -c icns <out.iconset> -o packaging/ramster.icns
// `make icon` runs both steps.

import AppKit

func rgb(_ hex: UInt32, _ a: CGFloat = 1) -> CGColor {
    CGColor(
        srgbRed: CGFloat((hex >> 16) & 0xff) / 255,
        green: CGFloat((hex >> 8) & 0xff) / 255,
        blue: CGFloat(hex & 0xff) / 255,
        alpha: a)
}

func gradient(_ top: CGColor, _ bottom: CGColor) -> CGGradient {
    CGGradient(
        colorsSpace: CGColorSpace(name: CGColorSpace.sRGB),
        colors: [bottom, top] as CFArray, locations: [0, 1])!
}

func rounded(_ r: CGRect, _ radius: CGFloat) -> CGPath {
    CGPath(roundedRect: r, cornerWidth: radius, cornerHeight: radius, transform: nil)
}

/// Fill `path` with a vertical gradient spanning `rect`.
func fill(_ ctx: CGContext, _ path: CGPath, _ rect: CGRect, _ top: CGColor, _ bottom: CGColor) {
    ctx.saveGState()
    ctx.addPath(path)
    ctx.clip()
    ctx.drawLinearGradient(
        gradient(top, bottom), start: CGPoint(x: 0, y: rect.minY),
        end: CGPoint(x: 0, y: rect.maxY), options: [])
    ctx.restoreGState()
}

/// Draws in a 1024x1024 space (origin bottom-left). `small` simplifies details
/// that turn into noise at 16/32 px.
func draw(_ ctx: CGContext, small: Bool) {
    // Body: macOS icon grid (824 pt square, 100 pt margin), soft drop shadow.
    let body = CGRect(x: 100, y: 100, width: 824, height: 824)
    let bodyPath = rounded(body, 185)
    ctx.saveGState()
    ctx.setShadow(offset: CGSize(width: 0, height: -10), blur: 22, color: rgb(0x000000, 0.35))
    ctx.addPath(bodyPath)
    ctx.setFillColor(rgb(0x2A3290))
    ctx.fillPath()
    ctx.restoreGState()
    fill(ctx, bodyPath, body, rgb(0x5D6CE4), rgb(0x1F2463))

    // Memory stick: PCB with side notches and a key notch in the contact edge.
    let stick = CGRect(x: 172, y: 362, width: 680, height: 300)
    var pcb = rounded(stick, 26)
    let notchR: CGFloat = small ? 0 : 22
    if notchR > 0 {
        let side = CGMutablePath()
        side.addEllipse(in: CGRect(x: stick.minX - notchR, y: stick.midY - notchR, width: 2 * notchR, height: 2 * notchR))
        side.addEllipse(in: CGRect(x: stick.maxX - notchR, y: stick.midY - notchR, width: 2 * notchR, height: 2 * notchR))
        pcb = pcb.subtracting(side)
    }
    let keyX = stick.minX + stick.width * 0.42
    let key = rounded(CGRect(x: keyX - 13, y: stick.minY - 30, width: 26, height: 30 + 46), 8)
    pcb = pcb.subtracting(key)

    ctx.saveGState()
    ctx.setShadow(offset: CGSize(width: 0, height: -14), blur: 26, color: rgb(0x000000, 0.4))
    ctx.addPath(pcb)
    ctx.setFillColor(rgb(0x1E9A55))
    ctx.fillPath()
    ctx.restoreGState()
    fill(ctx, pcb, stick, rgb(0x34C77B), rgb(0x168246))

    // Gold contacts along the bottom edge, skipping the key notch.
    let contactsTop = stick.minY + 58
    let n = small ? 8 : 22
    let pitch = (stick.width - 56) / CGFloat(n)
    let w = pitch * 0.58
    ctx.saveGState()
    ctx.addPath(pcb)
    ctx.clip()
    for i in 0..<n {
        let x = stick.minX + 28 + CGFloat(i) * pitch + (pitch - w) / 2
        if abs((x + w / 2) - keyX) < 24 { continue }
        let r = CGRect(x: x, y: stick.minY - 1, width: w, height: contactsTop - stick.minY)
        fill(ctx, rounded(r, min(w / 2, 6)), r, rgb(0xFAD46B), rgb(0xD39A2A))
    }
    ctx.restoreGState()

    // Memory chips.
    let chipW: CGFloat = 122
    let gap: CGFloat = (stick.width - 64 - 4 * chipW) / 3
    for i in 0..<4 {
        let r = CGRect(x: stick.minX + 32 + CGFloat(i) * (chipW + gap), y: stick.minY + 92, width: chipW, height: 156)
        let chip = rounded(r, 12)
        fill(ctx, chip, r, rgb(0x2C3446), rgb(0x141925))
        if !small {
            // Subtle top-edge highlight and pin-1 dot.
            ctx.saveGState()
            ctx.addPath(chip)
            ctx.clip()
            ctx.setFillColor(rgb(0xFFFFFF, 0.10))
            ctx.fill(CGRect(x: r.minX, y: r.maxY - 10, width: r.width, height: 10))
            ctx.restoreGState()
            ctx.setFillColor(rgb(0xFFFFFF, 0.22))
            ctx.fillEllipse(in: CGRect(x: r.minX + 16, y: r.maxY - 34, width: 12, height: 12))
        }
    }

    // Thin trace line above the chips.
    if !small {
        let trace = CGRect(x: stick.minX + 32, y: stick.maxY - 30, width: stick.width - 64, height: 8)
        ctx.addPath(rounded(trace, 4))
        ctx.setFillColor(rgb(0xFFFFFF, 0.18))
        ctx.fillPath()
    }
}

func png(_ px: Int) -> Data {
    let ctx = CGContext(
        data: nil, width: px, height: px, bitsPerComponent: 8, bytesPerRow: 0,
        space: CGColorSpace(name: CGColorSpace.sRGB)!,
        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    ctx.interpolationQuality = .high
    ctx.scaleBy(x: CGFloat(px) / 1024, y: CGFloat(px) / 1024)
    draw(ctx, small: px <= 32)
    let rep = NSBitmapImageRep(cgImage: ctx.makeImage()!)
    return rep.representation(using: .png, properties: [:])!
}

let args = CommandLine.arguments
guard args.count >= 2 else {
    FileHandle.standardError.write("usage: make-icon.swift <out.iconset> [preview.png]\n".data(using: .utf8)!)
    exit(2)
}
let out = URL(fileURLWithPath: args[1])
try FileManager.default.createDirectory(at: out, withIntermediateDirectories: true)
for base in [16, 32, 128, 256, 512] {
    for scale in [1, 2] {
        let name = scale == 1 ? "icon_\(base)x\(base).png" : "icon_\(base)x\(base)@2x.png"
        try png(base * scale).write(to: out.appendingPathComponent(name))
    }
}
if args.count >= 3 {
    let preview = URL(fileURLWithPath: args[2])
    try FileManager.default.createDirectory(
        at: preview.deletingLastPathComponent(), withIntermediateDirectories: true)
    try png(512).write(to: preview)
}
print("wrote \(out.path)")
