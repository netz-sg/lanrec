# lanrec

*[English version](README.md)*

Nimmt einen Gaming-PC (Windows) über eine direkte Ethernet-Strecke auf einem
zweiten Rechner (macOS, Linux, Windows) auf. Alles bleibt auf der GPU: Capture,
Encode und Versand berühren den Systemspeicher nie, und der Empfänger dekodiert
nichts -- er schreibt nur.

> Der Empfänger hängt an nichts als `std`. Der Sender braucht Windows, D3D11 und
> eine NVIDIA-GPU.

## Zielkonfiguration

| | |
|---|---|
| Quelle | 2560x1440, Spiel mit 144-165 Hz, Aufnahme 60 fps |
| GPU | RTX 4060 Ti (Ada, AD106), 1x NVENC 8. Gen |
| Netz | 1 GbE Direktverbindung PC <-> Mac, statische IPs, MTU 9000 |
| Codec | HEVC, YUV 4:4:4 10-bit, Profil FREXT |
| Rate Control | CQP 14-16 (nicht CBR) |
| Erwartete Bitrate | 150-350 Mbit/s, also ~35 % der Leitung |

## Benutzung

Auf dem Empfänger (Mac, Linux, PC):

    cargo run --release -p lanrec-recv -- --listen 10.0.0.2:9000 --out-dir ~/Aufnahmen

Auf dem Gaming-PC:

    cargo run --release -p lanrec-cli -- send --to 10.0.0.2:9000 --seconds 600

Lokal aufnehmen, ohne Netz:

    lanrec record --seconds 30 --qp 14 -o out.hevc

Was die Maschine kann, und worüber gesendet werden soll:

    lanrec caps                        # was der Encoder wirklich unterstuetzt
    lanrec nics                        # Adapter, Link-Status, ausgehandelte Rate
    lanrec rename 24:4B:.. "Zum Mac"   # Adapter benennen
    lanrec monitors                    # aufnehmbare Displays
    lanrec capture --seconds 10        # nur messen, nichts kodieren

Die grafische Oberfläche (Tauri) zeigt Live-Vorschau, Adapter mit Link-Status,
Qualitätseinstellungen mit Bitratenschätzung und die benannten Verbindungen:

    cd app && npm install && npm run tauri dev

## Warum diese Entscheidungen

**HEVC 4:4:4 statt AV1.** AV1 ist auf Ada auf 4:2:0 beschränkt -- vom Treiber
abgefragt, nicht vermutet. AV1 wäre pro Bit effizienter, aber Effizienz ist hier
das Problem nicht: es sind ~600 Mbit/s Reserve auf der Leitung. Der sichtbare
Gewinn liegt in 4:4:4, weil Chroma-Subsampling HUD-Text, dünne Linien und
gesättigte Farbflächen zerstört. Das sieht man, den AV1-Vorteil bei dieser
Bitrate nicht.

**10-bit auch bei SDR.** Die Quelle ist 8-bit BGRA, aber die zusätzliche
Präzision nutzt der *Encoder* -- und genau dort entsteht Banding in Verläufen.

**CQP statt CBR.** Konstante Qualität statt konstanter Bitrate. Ruhige Szenen
belegen weniger Leitung, Explosionen dürfen ausschlagen. Es gibt kein
Bitratenziel zu treffen, weil die Leitung nicht der Engpass ist.

**TCP statt SRT.** Ursprünglich war SRT geplant, und über einen Switch oder eine
WAN-Strecke bleibt es die richtige Antwort. Auf einem Direktkabel zwischen zwei
Maschinen gibt es aber keinen konkurrierenden Verkehr, keine Congestion und
praktisch keinen Verlust -- SRTs Retransmission-Maschinerie bringt dort nichts
und kostet eine C-Abhängigkeit auf beiden Plattformen. Eine Aufnahme interessiert
sich auch nicht für die Latenzspitze eines TCP-Retransmits, sondern dafür, dass
jedes Byte ankommt. Das Framing ist transportneutral; ein Wechsel auf SRT
berührt nur den Socket.

## Auf dieser GPU geprüft

RTX 4060 Ti (Ada, AD106), NVENC-API 13.1, eine Encoder-Engine:

| Codec | 4:4:4 | 10-bit | lossless | max |
|---|---|---|---|---|
| HEVC | ja | ja | ja | 8192x8192 |
| AV1 | **nein** | ja | nein | 8192x8192 |
| H.264 | ja | nein | ja | 4096x4096 |

Die Werte werden zur Laufzeit beim Treiber erfragt, nicht aus dem Modellnamen
abgeleitet. Die UI bietet dadurch nur Einstellungen an, die die
Encoder-Initialisierung überleben.

## WGC liefert nur bei Bildänderung

Gemessen: ein ruhender 165-Hz-Desktop liefert rund **48 Frames pro Sekunde**,
nicht 165. Windows.Graphics.Capture hängt am Compositor, nicht am Vblank -- wo
sich nichts ändert, gibt es nichts zu liefern.

Für einen Strom mit konstanter Bildrate heißt das: leere Slots müssen selbst
gefüllt werden, sonst wird die Aufnahme eines pausierten Spiels zur Lücke im
Zeitstrahl und alles danach verschiebt sich. Der Pacer meldet solche Slots als
`gap_slots`; der Consumer kodiert den vorherigen Frame erneut. Wiederholungen
identischen Inhalts kosten fast keine Bitrate.

    lanrec capture --seconds 6 --fps 60

    Eingang       289 Frames  (48.2 fps)
    Ausgang       360 Frames  (60.0 fps)
      davon neu   289
      Wiederholt   71

## Das 165-zu-60-Problem

165 / 60 = 2,75, kein ganzzahliges Verhältnis. Jede Aufnahme mit 60 fps aus einer
165-Hz-Quelle bekommt ein periodisches Mikro-Ruckeln, weil abwechselnd 2 und 3
Frames verworfen werden. Das ist nicht behebbar, nur verteilbar.

Die Frame-Auswahl ist deshalb timestamp-basiert und hält einen Frame zurück: für
jeden Slot gewinnt der nähere der beiden Nachbarn. Das halbiert den
Auswahlfehler auf +/-3 ms und macht ihn gleichmäßig, zum Preis eines einzigen
Eingangsframes Latenz.

Sauberste Lösung bleibt: Spiel auf 120 fps cappen -> 2:1 -> perfekt glatt.

## Architektur

    Gaming-PC (Windows)                        Empfaenger
    -------------------                        ----------
    WGC -> ID3D11Texture2D (bleibt auf GPU)
      |
    pace  (Timestamp-Grid, Luecken melden)
      |
    NVENC (D3D11-Textur als Input, Zero-Copy)
      |
    wire  [magic|version|kind|pts|dts|flags|len][payload]
      |
    TCP  ------------------------------------>  TCP
                                                  |
                                                Datei (kein Re-Encode)

### Crates

    crates/lanrec-wire/    Rahmenformat. Keine Plattformabhaengigkeiten.
    crates/lanrec-core/    Capture, Encode, Pacing, Vorschau, Adapter. Windows.
    crates/lanrec-cli/     Sender, headless.
    crates/lanrec-recv/    Empfaenger. Baut ueberall.
    app/                   Tauri 2 + React. Oberflaeche.
    vendor/                nv-codec-headers (NVENC-API, MIT) -- siehe NOTICE.md

Der Kern hängt bewusst an keiner UI. Wenn eine Aufnahme Frames verliert, ist die
erste Frage immer, ob die App oder die Pipeline schuld ist -- deshalb muss sich
letztere ohne Fenster starten und messen lassen.

### Eigenes Framing statt MPEG-TS

Beide Enden sind unter eigener Kontrolle. MPEG-TS würde PCR-Handling,
188-Byte-Padding und ein 90-kHz-PTS-Raster aufzwingen. Der 32-Byte-Header behält
Nanosekunden-Timestamps und lässt Platz für Metadaten, für die ein Container
keinen Ort hätte.

## Zwei Fallen, die M1 gekostet hat

**Ein Immediate Context, zwei Threads.** Capture läuft auf einem
Threadpool-Thread, der Encoder auf dem Aufrufer. Beide brauchen denselben
`ID3D11DeviceContext`, und der ist ausdrücklich nicht nebenläufig benutzbar. Ein
geklonter Context statt eines gemeinsamen Locks legt den Treiber lahm -- ohne
Fehler, ohne Absturz, der Prozess macht einfach keinen Fortschritt mehr. `Gpu`
gibt deshalb nur noch den Lock heraus, nie den nackten Context.

**`nvEncLockBitstream` blockiert auf leerem Puffer.** Im synchronen Betrieb ohne
B-Frames wird nach jedem Encode gesperrt und geleert, es bleibt also nie etwas
hängen. Ein zusätzliches Lock beim EOS-Flush meldet dann nicht "leer", sondern
wartet ewig auf Daten, die nie kommen.

## Bekannte Problemstellen

1. **Farbkonvertierung liegt beim Treiber.** NVENC bekommt BGRA und macht
   RGB->YUV selbst. Welche Matrix und welcher Wertebereich dabei benutzt werden,
   ist nicht unter unserer Kontrolle -- für ein Werkzeug mit dem Anspruch "beste
   Qualität" ist das die nächste offene Stelle. Ein Compute-Shader, der die
   Konvertierung explizit macht, löst das.
2. **Audio fehlt.** Und damit die schwierigste Stelle des ganzen Projekts: die
   Soundkarte läuft nicht exakt auf 48 kHz und ihre Uhr hat nichts mit QPC zu
   tun. Ohne Drift-Korrektur ist der Ton nach ~40 min um 200-400 ms versetzt.
3. **WGC-Session bricht ab** bei Alt-Tab, Auflösungswechsel, Fullscreen-Toggle.
   Muss neu aufgebaut werden, ohne die Aufnahme zu unterbrechen.
4. **Das Bitratenmodell ist geschätzt.** Log-linear, mit festen Faktoren für
   Chroma, Tiefe und Codec. Es unterscheidet 150 von 400 Mbit/s zuverlässig, aber
   nicht 230 von 250. Erst Messungen unter echter Spiellast kalibrieren es.

## Meilensteine

- [x] **M1** WGC -> NVENC -> lokale `.hevc`, Bitstrom auf NAL-Ebene verifiziert
- [x] **M2** Framing + TCP, Empfänger schreibt die Datei
- [x] Live-Vorschau (GPU-Downscale über die Mip-Kette, JPEG in die UI)
- [ ] **M3** WASAPI-Loopback-Audio, A/V-Sync, Drift-Korrektur
- [ ] **M4** Steuerkanal, Discovery, Reconnect
- [ ] Eigene Farbkonvertierung, Bitratenmodell kalibrieren

## Build

Voraussetzungen: siehe [`docs/setup.md`](docs/setup.md).

    cargo build --release          # Kern, Sender, Empfaenger
    cargo test --workspace

    cd app && npm install
    npm run tauri dev

## Lizenz

MIT, siehe [LICENSE](LICENSE). Fremdcode siehe [NOTICE.md](NOTICE.md).
