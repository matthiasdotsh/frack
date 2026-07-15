# NixOS VM test that boots frack in a cage kiosk and captures the
# screenshots referenced by the README. The driver drops the PNGs into
# the test's $out, so `nix build .#checks.<system>.screenshots` yields
# fresh images and `nix run .#update-screenshots` copies them into
# assets/screenshots/ — keeping the README pictures in sync with the
# code instead of rotting.
#
# `testers` (and thus the VMs' pkgs) is expected to come from an
# overlay-extended pkgs so the guest resolves `pkgs.frack` from its own
# package set — see the flake. That keeps host and guest packages
# separate without hardcoding a host-built derivation into the nodes.
#
# Explicit non-pkgs argument:
#   sample-scores - directory with the demo PDFs shown in the shots
{
  lib,
  testers,
  sample-scores,
}:

testers.runNixOSTest {
  name = "frack-screenshots";

  # wait_for_text / get_screen_text drive the synchronization
  # (no sleeps in the dark).
  enableOCR = true;

  # Pillow backs the pixel probes: OCR cannot read the small overlay
  # label and cannot see pen strokes, so those two states are detected
  # by sampling screenshot pixels instead.
  extraPythonPackages = p: [ p.pillow ];

  nodes.machine =
    { pkgs, ... }:
    let
      # Pre-seeded config: without it the library view would show the
      # "Neue Config angelegt" notice in every screenshot.
      configFile = pkgs.writeText "frack-config.toml" ''
        root_dir = "/home/alice/scores"
      '';

      # No blinking caret: screenshots must be byte-stable so git only
      # shows them as modified when the UI really changed.
      gtkSettings = pkgs.writeText "gtk-settings.ini" ''
        [Settings]
        gtk-cursor-blink=false
      '';

      # See the vpointer service below: moves are absolute pixels.
      vpointerDaemon = pkgs.writeText "vpointer.py" ''
        import os

        from evdev import AbsInfo, UInput, ecodes as e

        cap = {
            e.EV_KEY: [e.BTN_LEFT],
            e.EV_ABS: [
                (e.ABS_X, AbsInfo(0, 0, 799, 0, 0, 0)),
                (e.ABS_Y, AbsInfo(0, 0, 1279, 0, 0, 0)),
            ],
        }
        fifo = "/run/vpointer.fifo"
        if not os.path.exists(fifo):
            os.mkfifo(fifo)
        with UInput(cap, name="frack-test-pointer") as ui:
            while True:
                with open(fifo) as f:
                    for line in f:
                        parts = line.split()
                        if not parts:
                            continue
                        if parts[0] == "m":
                            ui.write(e.EV_ABS, e.ABS_X, int(parts[1]))
                            ui.write(e.EV_ABS, e.ABS_Y, int(parts[2]))
                        elif parts[0] == "d":
                            ui.write(e.EV_KEY, e.BTN_LEFT, 1)
                        elif parts[0] == "u":
                            ui.write(e.EV_KEY, e.BTN_LEFT, 0)
                        ui.syn()
      '';
    in
    {
      # A minimal graphical session: cage runs a single kiosk script,
      # which seeds alice's config files itself (systemd tmpfiles
      # refuses to write into foreign home directories) and works on a
      # writable copy of the scores so annotations can be burned in.
      services.cage = {
        enable = true;
        user = "alice";
        program = pkgs.writeShellScript "frack-kiosk" ''
          mkdir -p "$HOME/.config/frack" "$HOME/.config/gtk-4.0"
          install -m644 ${configFile} "$HOME/.config/frack/config.toml"
          install -m644 ${gtkSettings} "$HOME/.config/gtk-4.0/settings.ini"
          cp -rT ${sample-scores} "$HOME/scores"
          chmod -R u+w "$HOME/scores"
          exec ${lib.getExe pkgs.frack} "$HOME/scores"
        '';
      };
      users.users.alice = {
        isNormalUser = true;
        uid = 1000;
      };

      # Pointer control: a tiny daemon exposes an absolute uinput
      # pointer driven through a FIFO. Absolute coordinates map 1:1 to
      # output pixels and bypass libinput's pointer acceleration, which
      # is what both QEMU's monitor mouse (motion events eaten by its
      # input mux) and ydotool (absolute mode detours via 0,0 and gets
      # accelerated) failed to deliver.
      systemd.services.vpointer = {
        description = "Absolute virtual pointer for the test driver";
        wantedBy = [ "multi-user.target" ];
        serviceConfig.ExecStart = "${pkgs.python3.withPackages (p: [ p.evdev ])}/bin/python3 ${vpointerDaemon}";
      };

      # Microphone for the tuner: an ALSA loopback carries an endless
      # generated sine from the playback side to frack's capture side.
      # 443 Hz is the tuner's default reference pitch, so the reading
      # is a spot-on green.
      boot.kernelModules = [ "snd-aloop" ];
      environment.etc."asound.conf".text = ''
        pcm.!default {
          type asym
          playback.pcm "plughw:Loopback,0,0"
          capture.pcm "plughw:Loopback,1,0"
        }
      '';
      systemd.services.sine-source = {
        description = "Endless 443 Hz sine into the ALSA loopback";
        serviceConfig = {
          ExecStart = "${pkgs.alsa-utils}/bin/speaker-test -D plughw:Loopback,0,0 -c 1 -t sine -f 443";
          Restart = "always";
        };
      };

      fonts.packages = [ pkgs.dejavu_fonts ];
      # Headroom for GTK4 plus the parallel thumbnail workers; the
      # 1 GiB default is tight for that.
      virtualisation.memorySize = 2048;

      # On a loaded host the kiosk races udev/logind: cage can start
      # before logind has picked up the DRM card as a seat device, its
      # TakeDevice call then fails ("Failed to open device:
      # '/dev/dri/card0': No such device" — even though the node long
      # exists) and cage exits, tearing down the session before frack
      # ever ran. Seen on the Forgejo runner and reproduced locally
      # under build load; there is no unit to order against (DRM cards
      # get no systemd device unit), so let cage retry until the seat
      # is ready. A real crash loop is still caught and journal-dumped
      # by the frack fail-fast in the test script.
      systemd.services."cage-tty1" = {
        serviceConfig = {
          Restart = "on-failure";
          RestartSec = "1";
        };
        startLimitIntervalSec = 0;
      };
      # Portrait, like a tablet on a music stand — the page fills the
      # width instead of floating between black bars. The preferred
      # mode comes from the VGA device's xres/yres (its default is
      # 1280x800); virtualisation.resolution only affects GRUB/X11 and
      # would not reach the Wayland kiosk.
      virtualisation.qemu.options = [
        "-vga none"
        "-device VGA,xres=800,yres=1280"
      ];
    };

  # The script drives the real UI through keyboard and pointer: search
  # field, list navigation, page turning, tuner, overlay and freehand
  # annotation — so the screenshots double as an end-to-end test of all
  # of those paths.
  testScript = ''
    import os
    import time

    from PIL import Image

    start_all()

    machine.wait_for_unit("graphical.target")
    # Fail fast with the journal if the kiosk session died right after
    # boot (seen once on a KVM runner); without this the first OCR wait
    # times out after 900 s with no hint at the cause.
    try:
        machine.wait_until_succeeds("pgrep -u alice frack", timeout=60)
    except Exception:
        print(machine.execute("journalctl -b --no-pager | tail -n 200")[1])
        raise

    machine.wait_for_unit("vpointer.service")
    machine.wait_until_succeeds("test -p /run/vpointer.fifo")


    def vp(cmd: str) -> None:
        machine.succeed(f"echo '{cmd}' > /run/vpointer.fifo")


    def probe_image() -> "Image.Image":
        # Grab the current frame as a temporary screenshot; the probe
        # file must not linger in $out or the up-to-date check would
        # flag it as an uncommitted image.
        path = os.environ["out"] + "/probe.png"
        machine.screenshot("probe")
        img = Image.open(path).convert("RGB")
        os.remove(path)
        return img


    def mouse_move(x: float, y: float) -> None:
        vp(f"m {round(x)} {round(y)}")


    def park_cursor() -> None:
        # Park the pointer in the bottom-right corner so it does not
        # sit in the middle of every screenshot.
        mouse_move(795, 1275)


    def click_at(x: float, y: float) -> None:
        # Generous delays: under TCG emulation the guest needs a moment
        # to deliver each event before the next one arrives.
        mouse_move(x, y)
        time.sleep(1)
        vp("d")  # left press
        time.sleep(1)
        vp("u")  # release


    def stroke(points: list[tuple[float, float]]) -> None:
        # One freehand stroke: press, drag through the points, release.
        # The sleeps spread the motions over several compositor frames
        # so GTK records them as separate points.
        mouse_move(*points[0])
        time.sleep(0.5)
        vp("d")
        for point in points[1:]:
            time.sleep(0.2)
            mouse_move(*point)
        time.sleep(0.5)
        vp("u")
        time.sleep(0.5)


    with subtest("library lists the sample scores"):
        machine.wait_for_text("(?i)brahms")
        park_cursor()
        machine.screenshot("library")

    with subtest("typing in the search filters the list"):
        # The search entry is focused on startup.
        machine.send_chars("bass")

        def filtered(_last_try: bool) -> bool:
            text = machine.get_screen_text().lower()
            return "bass" in text and "tenor" not in text and "alto" not in text

        retry(filtered)

    with subtest("a piece opens from the keyboard"):
        machine.send_key("down")  # focus moves from the search to the list
        machine.send_key("ret")
        machine.wait_for_text("(?i)symphony")
        # The large "Johannes Brahms" title sits at the top of page 1
        # and is unique to it — the page-2 header says "Brahms —
        # Symphony..." and "TROMBONE III (BASS)", so those strings
        # appear on both pages. Used below to detect the half-page
        # turn, so make sure OCR sees it at all. No screenshot here:
        # the annotation shot shows the same full-page view (plus the
        # annotation).
        machine.wait_for_text("(?i)johannes")

    with subtest("freehand annotation: 'Choral' above letter C"):
        machine.send_key("a")  # pen mode (mouse strokes draw)

        # Simple polyline letters in a 24 px box: (strokes, advance).
        letters = {
            "C": ([[(13, 3), (7, 0), (2, 5), (0, 12), (2, 19), (7, 24), (13, 21)]], 17),
            "h": ([[(0, 0), (0, 24)], [(0, 13), (4, 9), (8, 10), (9, 14), (9, 24)]], 13),
            "o": ([[(4, 10), (0, 14), (0, 20), (4, 24), (8, 20), (8, 14), (4, 10)]], 12),
            "r": ([[(0, 9), (0, 24)], [(0, 15), (3, 10), (7, 9)]], 11),
            "a": ([[(8, 11), (3, 9), (0, 14), (0, 20), (3, 24), (8, 22)], [(8, 9), (8, 24)]], 12),
            "l": ([[(1, 0), (1, 24)]], 6),
        }
        # Just right of the boxed rehearsal letter C, above "p dolce".
        # In portrait the page fills the 800 px width (previously 608 px
        # between black bars), so the old landscape anchor and the 24 px
        # letter boxes are scaled by 800/608 and shifted by the vertical
        # offset of the centered page.
        s = 800 / 608
        x = 260 * s
        top = 288 * s + 114
        for ch in "Choral":
            strokes, advance = letters[ch]
            for st in strokes:
                stroke([(x + dx * s, top + dy * s) for dx, dy in st])
            x += (advance + 4) * s


        def ink_visible(_last_try: bool) -> bool:
            # Look for red pen pixels in the written area.
            data = probe_image().crop((335, 485, 510, 540)).tobytes()
            return any(
                data[i] > 140 and data[i + 1] < 110 and data[i + 2] < 110
                for i in range(0, len(data), 3)
            )

        retry(ink_visible)
        park_cursor()
        machine.screenshot("annotation")
        # Leaving pen mode burns the strokes into the (writable) PDF;
        # later screenshots show the annotation persisted.
        machine.send_key("a")

    with subtest("the tuner reads a green 443 Hz from the loopback mic"):
        machine.systemctl("start sine-source")
        machine.send_key("t")
        machine.wait_for_text("443")
        # The pitch history is a fixed 8 s window scrolling by design;
        # give it time to fill completely so the graph spans the bar.
        machine.sleep(9)

    with subtest("a pedal press turns half a page (tuner still shown)"):
        # The top half of page 2 replaces the top of the view while the
        # bottom half of page 1 stays — so the "Johannes Brahms" title
        # at the top of page 1 disappears. (Do not key this on the
        # trombone header: both pages carry one, and whether OCR picks
        # up the smaller page-2 copy is a coin toss.)
        machine.send_key("pgdn")

        def half_page_shown(_last_try: bool) -> bool:
            return "johannes" not in machine.get_screen_text().lower()

        retry(half_page_shown)
        machine.screenshot("tuner-half-page")
        machine.send_key("t")  # tuner off
        machine.send_key("pgup")  # back to the full first page
        machine.wait_for_text("(?i)johannes")

    with subtest("a middle tap opens the touch overlay"):
        click_at(400, 640)

        # The overlay is an OSD bar with its own dark translucent
        # background anchored to the window bottom. Below the fitted page
        # that region is now plain paper white (the letterbox), so any
        # dark pixel there means the overlay has opened (its labels are
        # too small for OCR). Gating on this also makes the screenshot
        # wait until the bar has actually rendered.
        def overlay_open(_last_try: bool) -> bool:
            data = probe_image().crop((25, 1172, 400, 1270)).tobytes()
            return any(b < 90 for b in data)

        retry(overlay_open)
        park_cursor()  # moving the pointer does not close the overlay
        machine.screenshot("overlay")
        machine.send_key("esc")  # hides the overlay

  '';
}
