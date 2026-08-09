; Swapmod plate changer, Bambu Lab A1 mini: eject the current plate, load the next.
;
; Provenance, exactly: the 32 motion commands below are the vendor's own
; ';swap-s start 1' .. ';swap-s end' block from Metadata/plate_1.gcode of
; Self-Test_A1M-STL_Bambu-Studio_v02.gcode.3mf, verbatim and with nothing else
; from that file. `G90` and `G28` are added here as preconditions — absolute
; positioning and a known origin, without which the coordinates mean nothing.
;
; The changer has no electronics: the toolhead drives it. The trigger is a
; ROPE, pulled by the Z axis dropping 186->180 at X=188 and again at X=170;
; everything else is positioning. `G0 Y150 F200` is deliberately slow — that is
; where the load is taken.
;
; About the dwells: the self-test's own later cycles use `G4 S1` and fewer of
; them, and it completes ten swaps that way, so `S3` is the cautious setting
; rather than a requirement. This block is the one that has actually been run
; on the machine (2026-08-07, a full print -> eject -> load -> print cycle), so
; it is the one shipped, unshortened.
;
; These coordinates are for THIS machine and this accessory. Read them before
; you run them.
G90
G28
G0 Z160 F5000
G0 X170 F5000
G0 Z180 F2000
G4 S3
G0 Y186.5 F3000
G4 S3
G0 Z186 F2000
G0 X188 F5000
G0 Z180
G4 S3
G0 Y150 F200
G4 S3
G0 Y-6 F2000
G4 S3
G0 Z186 F5000
G0 X170
G0 Z180 F5000
G4 S3
G0 Y150 F2000
G4 S3
G0 Y15 F3000
G4 S3
G0 Y180 F2000
G0 Y186.5 F500
G4 S3
G0 Y5 F5000
G4 S3
G0 Y-6 F200
G4 S3
G0 Y5 F500
G4 S3
G0 Y100 F5000
