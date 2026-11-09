import Cocoa
import CoreGraphics

// Synthesizes keyboard input to the frontmost app. Requires Accessibility
// trust; checks first and exits 2 if not granted so callers can tell.
guard AXIsProcessTrusted() else {
    FileHandle.standardError.write("NOT_TRUSTED\n".data(using: .utf8)!)
    exit(2)
}

let args = Array(CommandLine.arguments.dropFirst())
let src = CGEventSource(stateID: .hidSystemState)

func post(_ key: CGKeyCode, flags: CGEventFlags = []) {
    let down = CGEvent(keyboardEventSource: src, virtualKey: key, keyDown: true)!
    down.flags = flags
    down.post(tap: .cghidEventTap)
    usleep(12_000)
    let up = CGEvent(keyboardEventSource: src, virtualKey: key, keyDown: false)!
    up.flags = flags
    up.post(tap: .cghidEventTap)
    usleep(12_000)
}

// US-QWERTY virtual key for an ASCII character: (keycode, needs shift).
// Virtual-key events proved 100% reliable across every test run, while
// unicode-string events are dropped until the target's text-input session
// is warm; so ASCII goes through real keycodes and only non-ASCII falls
// back to unicode strings.
func vkFor(_ ch: Character) -> (CGKeyCode, Bool)? {
    let plain: [Character: CGKeyCode] = [
        "a":0,"s":1,"d":2,"f":3,"h":4,"g":5,"z":6,"x":7,"c":8,"v":9,"b":11,
        "q":12,"w":13,"e":14,"r":15,"y":16,"t":17,"1":18,"2":19,"3":20,"4":21,
        "6":22,"5":23,"=":24,"9":25,"7":26,"-":27,"8":28,"0":29,"]":30,"o":31,
        "u":32,"[":33,"i":34,"p":35,"l":37,"j":38,"'":39,"k":40,";":41,"\\":42,
        ",":43,"/":44,"n":45,"m":46,".":47,"\t":48," ":49,"`":50,"\n":36,
    ]
    let shifted: [Character: Character] = [
        "A":"a","B":"b","C":"c","D":"d","E":"e","F":"f","G":"g","H":"h","I":"i",
        "J":"j","K":"k","L":"l","M":"m","N":"n","O":"o","P":"p","Q":"q","R":"r",
        "S":"s","T":"t","U":"u","V":"v","W":"w","X":"x","Y":"y","Z":"z",
        "!":"1","@":"2","#":"3","$":"4","%":"5","^":"6","&":"7","*":"8","(":"9",
        ")":"0","_":"-","+":"=","{":"[","}":"]",":":";","\"":"'","|":"\\",
        "<":",",">":".","?":"/","~":"`",
    ]
    if let vk = plain[ch] { return (vk, false) }
    if let base = shifted[ch], let vk = plain[base] { return (vk, true) }
    return nil
}

func typeText(_ text: String) {
    for ch in text {
        if let (vk, shift) = vkFor(ch) {
            post(vk, flags: shift ? .maskShift : [])
        } else {
            // Non-ASCII: the unicode-string path, best effort.
            for scalar in String(ch).unicodeScalars {
                let down = CGEvent(keyboardEventSource: src, virtualKey: 0, keyDown: true)!
                var u = UniChar(scalar.value)
                down.keyboardSetUnicodeString(stringLength: 1, unicodeString: &u)
                down.post(tap: .cghidEventTap)
                usleep(10_000)
                let up = CGEvent(keyboardEventSource: src, virtualKey: 0, keyDown: false)!
                up.keyboardSetUnicodeString(stringLength: 1, unicodeString: &u)
                up.post(tap: .cghidEventTap)
                usleep(10_000)
            }
        }
    }
}

let special: [String: CGKeyCode] = [
    "return": 36, "tab": 48, "space": 49, "delete": 51, "escape": 53,
    "left": 123, "right": 124, "down": 125, "up": 126,
]

/// Named key, else any single ASCII character via the QWERTY table. A name
/// this cannot resolve is a fatal error — a silently-wrong keycode cost a
/// debugging round once already.
func keycode(_ name: String) -> CGKeyCode {
    if let vk = special[name] { return vk }
    if name.count == 1, let (vk, _) = vkFor(name.first!) { return vk }
    FileHandle.standardError.write("UNKNOWN_KEY \(name)\n".data(using: .utf8)!)
    exit(4)
}

var i = 0
while i < args.count {
    let arg = args[i]
    switch arg {
    case "--text":
        i += 1
        typeText(args[i])
    case "--key":
        i += 1
        post(keycode(args[i]))
    case "--cmd":
        i += 1
        post(keycode(args[i]), flags: .maskCommand)
    case "--cmd-shift":
        i += 1
        post(keycode(args[i]), flags: [.maskCommand, .maskShift])
    case "--shift-key":
        i += 1
        post(keycode(args[i]), flags: .maskShift)
    case "--activate":
        i += 1
        if let pid = Int32(args[i]),
           let app = NSRunningApplication(processIdentifier: pid) {
            // Activation is cooperative on modern macOS: request it, then
            // WAIT until it has actually happened, or keystrokes race into
            // whatever is still frontmost.
            var tries = 0
            while NSWorkspace.shared.frontmostApplication?.processIdentifier != pid && tries < 40 {
                app.activate()
                usleep(100_000)
                tries += 1
            }
            if NSWorkspace.shared.frontmostApplication?.processIdentifier != pid {
                FileHandle.standardError.write("ACTIVATE_FAILED\n".data(using: .utf8)!)
                exit(3)
            }
            usleep(150_000)
        }
    case "--mousedrag":
        // --mousedrag x1 y1 x2 y2, in global display points. A real drag:
        // press, cross the drag threshold in small steps, settle, release.
        let x1 = Double(args[i+1])!, y1 = Double(args[i+2])!
        let x2 = Double(args[i+3])!, y2 = Double(args[i+4])!
        i += 4
        let from = CGPoint(x: x1, y: y1)
        let to = CGPoint(x: x2, y: y2)
        let down = CGEvent(mouseEventSource: src, mouseType: .leftMouseDown,
                           mouseCursorPosition: from, mouseButton: .left)!
        down.post(tap: .cghidEventTap)
        usleep(120_000)
        let steps = 24
        for step in 1...steps {
            let t = Double(step) / Double(steps)
            let pt = CGPoint(x: x1 + (x2 - x1) * t, y: y1 + (y2 - y1) * t)
            let move = CGEvent(mouseEventSource: src, mouseType: .leftMouseDragged,
                               mouseCursorPosition: pt, mouseButton: .left)!
            move.post(tap: .cghidEventTap)
            usleep(18_000)
        }
        usleep(250_000)
        let up = CGEvent(mouseEventSource: src, mouseType: .leftMouseUp,
                         mouseCursorPosition: to, mouseButton: .left)!
        up.post(tap: .cghidEventTap)
        usleep(120_000)
    case "--click":
        let x = Double(args[i+1])!, y = Double(args[i+2])!
        i += 2
        let pt = CGPoint(x: x, y: y)
        for (kind, delay) in [(CGEventType.leftMouseDown, 60_000), (.leftMouseUp, 60_000)] {
            let ev = CGEvent(mouseEventSource: src, mouseType: kind,
                             mouseCursorPosition: pt, mouseButton: .left)!
            ev.post(tap: .cghidEventTap)
            usleep(UInt32(delay))
        }
    case "--sleep":
        i += 1
        usleep(UInt32((Double(args[i]) ?? 0.2) * 1_000_000))
    default:
        break
    }
    i += 1
}
print("SENT")
