# Setup

## Toolchain

    winget install --id Rustlang.Rustup -e
    winget install --id Microsoft.VisualStudio.2022.BuildTools -e --override "--quiet --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
    winget install --id LLVM.LLVM -e
    winget install --id OpenJS.NodeJS.LTS -e

- **Rustup** zieht die stable-MSVC-Toolchain (siehe `rust-toolchain.toml`).
- **VS Build Tools** liefern Linker und Windows SDK. Ohne die kann das
  MSVC-Target nicht linken. Braucht Admin-Rechte (UAC).
- **LLVM** wird von `bindgen` gebraucht (libclang), um die NVENC-Bindings aus
  `vendor/nv-codec-headers` zu erzeugen. Ohne gesetztes `LIBCLANG_PATH` findet
  bindgen die DLL unter Umstaenden nicht:

      setx LIBCLANG_PATH "C:\Program Files\LLVMin"

- **Node** wird nur fuer das Frontend gebraucht (Vite + React), nicht fuer den Kern.

Geprueft auf dieser Maschine: Rust 1.98.0, MSVC 14.44.35207, Windows SDK
10.0.26100.0, Node 24.19, WebView2 152.0.4191.53.

## NVENC

Die Bindings werden aus `vendor/nv-codec-headers/include/ffnvcodec/nvEncodeAPI.h`
generiert (MIT-lizenziert, kein NVIDIA-Account nötig).

Die eigentliche Runtime ist `nvEncodeAPI64.dll` und kommt mit dem Grafiktreiber.
Sie wird zur Laufzeit per `LoadLibrary` geladen, nicht gelinkt. Es gibt also
keine Link-Zeit-Abhängigkeit auf irgendein NVIDIA-SDK.

Geprüft auf dieser Maschine:

    NVENC API      13.1
    GPU            RTX 4060 Ti (AD106), 16 GB
    Treiber        616.56

## Netzwerk (ab M2)

Direktverbindung PC <-> Mac, kein Switch, kein DHCP:

| | PC | Mac |
|---|---|---|
| IP | 10.0.0.1/24 | 10.0.0.2/24 |
| MTU | 9000 | 9000 |

Jumbo Frames senken bei ~350 Mbit/s die Interrupt-Last deutlich. Beide Seiten
müssen 9000 gesetzt haben, sonst fragmentiert es.
