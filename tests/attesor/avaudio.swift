// AVAudioEngine: 440 Hz source node for SECS seconds, then AVAudioPlayer plays a generated WAV.
import AVFoundation
let secs = Double(ProcessInfo.processInfo.environment["SECS"] ?? "2")!
let e = AVAudioEngine(); var ph = 0.0, rendered = 0
let fmt = e.outputNode.inputFormat(forBus: 0); let sr = fmt.sampleRate
let src = AVAudioSourceNode { _, _, n, abl -> OSStatus in
  let l = UnsafeMutableAudioBufferListPointer(abl)
  for i in 0..<Int(n) { let v = Float(0.2 * sin(ph)); ph += 2 * .pi * 440 / sr; for b in l { b.mData!.assumingMemoryBound(to: Float.self)[i] = v } }
  rendered += Int(n); return noErr }
e.attach(src); e.connect(src, to: e.mainMixerNode, format: AVAudioFormat(standardFormatWithSampleRate: sr, channels: fmt.channelCount))
do { try e.start() } catch { print("engine start failed", error); exit(1) }
Thread.sleep(forTimeInterval: secs); e.stop()
print("engine sr", sr, "frames/s", Int(Double(rendered) / secs))
// WAV file → AVAudioPlayer (660 Hz)
let url = URL(fileURLWithPath: NSTemporaryDirectory() + "attesor-tone.wav")
do { let f = try! AVAudioFile(forWriting: url, settings: [AVFormatIDKey: kAudioFormatLinearPCM, AVSampleRateKey: 44100, AVNumberOfChannelsKey: 1])
let buf = AVAudioPCMBuffer(pcmFormat: f.processingFormat, frameCapacity: AVAudioFrameCount(44100 * secs))!; buf.frameLength = buf.frameCapacity
for i in 0..<Int(buf.frameLength) { buf.floatChannelData![0][i] = Float(0.2 * sin(2 * .pi * 660 * Double(i) / 44100)) }
try! f.write(from: buf) }  // AVAudioFile finalizes the file when released
let p = try! AVAudioPlayer(contentsOf: url); print("player duration", p.duration, "play", p.play())
Thread.sleep(forTimeInterval: secs + 0.3); print("player time", p.currentTime > 0 || !p.isPlaying ? "advanced" : "stuck")
