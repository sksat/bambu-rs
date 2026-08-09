; Swapmod plate changer, Bambu Lab A1 mini: load a plate, without ejecting one.
;
; For the FIRST plate of a run — the swap macro assumes a plate is already on
; the bed and ejects it. Verified on the machine (2026-08-06).
;
; Provenance, exactly: the vendor's own ';swap-s start plate load only - v05'
; block from Metadata/plate_1.gcode of
; Self-Test_A1M-STL_Bambu-Studio_v02.gcode.3mf, verbatim and with nothing else
; from that file. Its `G90`/`G28` are the vendor's, not added here.
;
; Unlike the swap, this does NOT touch the trigger: the head parks out of the
; way at X=-10 / Z=30 and stays there, and the bed's Y travel alone carries the
; plate in. Takes about 40 s.
;
; These coordinates are for THIS machine and this accessory. Read them before
; you run them.
G90
G28
G0 Z30 F5000
G0 X-10
G0 Y-6 F2000
G4 S3
G0 Y150
G4 S3
G0 Y100
G4 S3
G0 Y186.5
G4 S3
G0 Y-6
G4 S3
G0 Y5 F500
G4 S3
G0 Y100 F5000
G4 S3
