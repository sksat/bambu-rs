# Builds the DOOM frame source for `bambu serve --emulate-doom`.
#
# Nothing here is part of the bambu-rs build: run ./build.sh, which fetches
# doomgeneric and stb_image_write.h into ./build and then calls this.
#
#   make DG=build/doomgeneric/doomgeneric STB=build
#
# The object list is doomgeneric's own (its Makefile's SRC_DOOM) with the X11
# platform layer swapped for ours — which is why the checkout is pinned in
# build.sh rather than tracking master.

DG  ?= build/doomgeneric/doomgeneric
STB ?= build
OBJDIR ?= build/obj
OUT ?= build/bambu-doom-engine

CC ?= cc
# DOOMGENERIC_RESX/RESY is what doomgeneric renders into; 1280x800 is DOOM's
# 320x200 at a whole 4x, so nothing is resampled before our own letterboxing.
CFLAGS ?= -O2 -Wall -Wno-unused-result
CFLAGS += -DNORMALUNIX -DLINUX -D_DEFAULT_SOURCE -DDOOMGENERIC_RESX=1280 -DDOOMGENERIC_RESY=800
CFLAGS += -I$(DG) -I$(STB)
LIBS += -lm

SRC_DOOM = dummy.o am_map.o doomdef.o doomstat.o dstrings.o d_event.o d_items.o d_iwad.o \
	d_loop.o d_main.o d_mode.o d_net.o f_finale.o f_wipe.o g_game.o hu_lib.o hu_stuff.o \
	info.o i_cdmus.o i_endoom.o i_joystick.o i_scale.o i_sound.o i_system.o i_timer.o \
	memio.o m_argv.o m_bbox.o m_cheat.o m_config.o m_controls.o m_fixed.o m_menu.o \
	m_misc.o m_random.o p_ceilng.o p_doors.o p_enemy.o p_floor.o p_inter.o p_lights.o \
	p_map.o p_maputl.o p_mobj.o p_plats.o p_pspr.o p_saveg.o p_setup.o p_sight.o \
	p_spec.o p_switch.o p_telept.o p_tick.o p_user.o r_bsp.o r_data.o r_draw.o r_main.o \
	r_plane.o r_segs.o r_sky.o r_things.o sha1.o sounds.o statdump.o st_lib.o st_stuff.o \
	s_sound.o tables.o v_video.o wi_stuff.o w_checksum.o w_file.o w_main.o w_wad.o \
	z_zone.o w_file_stdc.o i_input.o i_video.o doomgeneric.o

OBJS = $(addprefix $(OBJDIR)/, $(SRC_DOOM)) $(OBJDIR)/doomgeneric_bambu.o

all: $(OUT)

$(OUT): $(OBJS)
	$(CC) $(CFLAGS) $(OBJS) -o $@ $(LIBS)

$(OBJDIR)/doomgeneric_bambu.o: doomgeneric_bambu.c | $(OBJDIR)
	$(CC) $(CFLAGS) -c $< -o $@

$(OBJDIR)/%.o: $(DG)/%.c | $(OBJDIR)
	$(CC) $(CFLAGS) -c $< -o $@

$(OBJDIR):
	mkdir -p $(OBJDIR)

clean:
	rm -rf $(OBJDIR) $(OUT)

.PHONY: all clean
