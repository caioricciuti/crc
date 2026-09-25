// Draws the background of the disk image window, with nothing installed:
// CoreGraphics and CoreText from the system, run by the Swift that comes
// with Xcode. Writes background.png (1x) and background@2x.png into the
// directory given; make-dmg.sh joins them into one Retina-aware TIFF.
//
// The window is 660 x 420 points. Finder puts crc.app at (180, 190) and
// Applications at (480, 190), icons 128 points, their names under them;
// this draws everything else: a light ground with the site's two glows,
// the arrow between the icons and one line of instruction.
//
// Light, not the site's graphite: Finder draws the icons' names in black
// over a background picture whatever the appearance, and no setting a
// script can reach changes that. Black names on graphite were unreadable.
//
// Usage: swift scripts/make-dmg-background.swift <out-dir>
import AppKit

let width: CGFloat = 660
let height: CGFloat = 420

func rgb(_ hex: UInt32, _ alpha: CGFloat = 1) -> CGColor {
    CGColor(
        srgbRed: CGFloat((hex >> 16) & 0xff) / 255,
        green: CGFloat((hex >> 8) & 0xff) / 255,
        blue: CGFloat(hex & 0xff) / 255,
        alpha: alpha)
}

// The editor's palette on a light ground: the light theme's surface, and
// the mint caret and function blue, deepened enough to read on it.
let ground = rgb(0xf5f7f6)
let mint = rgb(0x3fae80)
let blue = rgb(0x4c78e6)
let text = rgb(0x1b2120)
let dim = rgb(0x66716d)

func glow(_ ctx: CGContext, at center: CGPoint, radius: CGFloat, color: CGColor) {
    let clear = color.copy(alpha: 0)!
    let gradient = CGGradient(
        colorsSpace: CGColorSpace(name: CGColorSpace.sRGB),
        colors: [color, clear] as CFArray, locations: [0, 1])!
    ctx.drawRadialGradient(
        gradient, startCenter: center, startRadius: 0, endCenter: center,
        endRadius: radius, options: [])
}

/// A line of text centred on `x`, its baseline at `y` (top-left origin).
func label(_ ctx: CGContext, _ string: String, size: CGFloat, weight: NSFont.Weight,
           color: CGColor, x: CGFloat, y: CGFloat) {
    let font = NSFont.systemFont(ofSize: size, weight: weight)
    let attributed = NSAttributedString(
        string: string,
        attributes: [.font: font, .foregroundColor: NSColor(cgColor: color)!])
    let line = CTLineCreateWithAttributedString(attributed)
    let bounds = CTLineGetBoundsWithOptions(line, .useOpticalBounds)
    ctx.saveGState()
    // The context is flipped so y runs down; text has to be flipped back.
    ctx.textMatrix = CGAffineTransform(scaleX: 1, y: -1)
    ctx.textPosition = CGPoint(x: x - bounds.width / 2, y: y)
    CTLineDraw(line, ctx)
    ctx.restoreGState()
}

func render(scale: CGFloat, to url: URL) throws {
    let ctx = CGContext(
        data: nil, width: Int(width * scale), height: Int(height * scale),
        bitsPerComponent: 8, bytesPerRow: 0,
        space: CGColorSpace(name: CGColorSpace.sRGB)!,
        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    ctx.scaleBy(x: scale, y: scale)
    ctx.translateBy(x: 0, y: height)
    ctx.scaleBy(x: 1, y: -1)

    ctx.setFillColor(ground)
    ctx.fill(CGRect(x: 0, y: 0, width: width, height: height))
    glow(ctx, at: CGPoint(x: 150, y: 110), radius: 320, color: mint.copy(alpha: 0.16)!)
    glow(ctx, at: CGPoint(x: 530, y: 340), radius: 330, color: blue.copy(alpha: 0.14)!)

    // The arrow, mint to blue, between the two icons' edges.
    let y: CGFloat = 186
    let from: CGFloat = 268
    let to: CGFloat = 392
    let path = CGMutablePath()
    path.move(to: CGPoint(x: from, y: y))
    path.addLine(to: CGPoint(x: to, y: y))
    path.move(to: CGPoint(x: to - 13, y: y - 12))
    path.addLine(to: CGPoint(x: to, y: y))
    path.addLine(to: CGPoint(x: to - 13, y: y + 12))
    ctx.saveGState()
    ctx.setLineWidth(3.5)
    ctx.setLineCap(.round)
    ctx.setLineJoin(.round)
    ctx.addPath(path)
    ctx.replacePathWithStrokedPath()
    ctx.clip()
    let stroke = CGGradient(
        colorsSpace: CGColorSpace(name: CGColorSpace.sRGB),
        colors: [mint.copy(alpha: 0.9)!, blue] as CFArray,
        locations: [0, 1])!
    ctx.drawLinearGradient(
        stroke, start: CGPoint(x: from, y: y), end: CGPoint(x: to, y: y), options: [])
    ctx.restoreGState()

    label(ctx, "Drag crc to Applications", size: 16, weight: .semibold,
          color: text, x: width / 2, y: 338)
    label(ctx, "then open it from Launchpad or Spotlight",
          size: 12.5, weight: .regular, color: dim, x: width / 2, y: 362)

    let image = ctx.makeImage()!
    let rep = NSBitmapImageRep(cgImage: image)
    rep.size = NSSize(width: width, height: height)
    try rep.representation(using: .png, properties: [:])!.write(to: url)
}

let out = URL(fileURLWithPath: CommandLine.arguments[1], isDirectory: true)
try render(scale: 1, to: out.appendingPathComponent("background.png"))
try render(scale: 2, to: out.appendingPathComponent("background@2x.png"))
