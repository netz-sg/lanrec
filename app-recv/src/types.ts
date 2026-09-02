/** Mirrors `lanrec_recv::Status` and the app's `View`. */
export interface Status {
  listeningOn: string;
  peer: string | null;
  codec: string | null;
  width: number;
  height: number;
  fps: number;
  chroma: string | null;
  bitDepth: number;
  source: string | null;

  frames: number;
  keyframes: number;
  bytes: number;
  seconds: number;
  bitrateBps: number;
  path: string | null;

  finished: boolean;
  cleanEnd: boolean;
  error: string | null;
}

export interface View {
  running: boolean;
  outDir: string;
  status: Status | null;
  fatal: string | null;
}
