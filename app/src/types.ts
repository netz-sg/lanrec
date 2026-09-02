/** Mirrors the serde shapes in `lanrec-core`. The Rust side owns these; this is
 *  only the reader. */

export type Medium = "ethernet" | "wifi" | "loopback" | "other";
export type Suitability = "good" | "marginal" | "unusable";

export interface NicView {
  index: number;
  /** The name Windows gives the adapter. */
  name: string;
  /** The name the user gave it, if any. */
  label: string | null;
  /** What to show: the user's name when set, otherwise the Windows one. */
  displayName: string;
  description: string;
  mac: string | null;
  medium: Medium;
  up: boolean;
  linkSpeedBps: number;
  linkSpeedLabel: string;
  mtu: number;
  jumbo: boolean;
  ipv4: string[];
  hasGateway: boolean;
  suitability: Suitability;
  note: string | null;
  directLinkCandidate: boolean;
}

export type Codec = "h264" | "hevc" | "av1";
export type Chroma = "yuv420" | "yuv444";
export type BitDepth = "eight" | "ten";

export interface CodecCaps {
  codec: Codec;
  label: string;
  yuv444: boolean;
  tenBit: boolean;
  lossless: boolean;
  maxWidth: number;
  maxHeight: number;
}

export interface GpuCaps {
  adapter: string;
  encoderEngines: number;
  codecs: CodecCaps[];
}

export type RateControl =
  | { mode: "cqp"; qp: number }
  | { mode: "vbr"; targetBps: number; maxBps: number };

export interface Profile {
  codec: Codec;
  chroma: Chroma;
  depth: BitDepth;
  width: number;
  height: number;
  fpsNum: number;
  fpsDen: number;
  rateControl: RateControl;
  gopSeconds: number;
}

export interface Issue {
  blocking: boolean;
  message: string;
}

export interface Evaluation {
  estimatedBps: number;
  issues: Issue[];
}

export interface MonitorView {
  index: number;
  device: string;
  width: number;
  height: number;
  primary: boolean;
}

export interface PreviewFrame {
  /** `data:image/jpeg;base64,...` */
  dataUrl: string;
  width: number;
  height: number;
}
