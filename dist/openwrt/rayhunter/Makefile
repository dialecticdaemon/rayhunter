# OpenWrt package for the GL.iNet Mudi V2. Build the daemon first (see doc/glinet-mudi.md) and pass
# it as RAYHUNTER_BIN, e.g. from the OpenWrt 22.03.4 SDK for ath79/nand:
#   ln -s <repo>/dist/openwrt/rayhunter package/rayhunter
#   make package/rayhunter/compile RAYHUNTER_BIN=<repo>/target/mips-unknown-linux-musl/firmware/rayhunter-daemon
include $(TOPDIR)/rules.mk

PKG_NAME:=rayhunter
PKG_VERSION:=0.13.0
PKG_RELEASE:=1
PKG_LICENSE:=GPL-3.0

# ship the daemon exactly as built, without OpenWrt stripping it again
STRIP:=:
RSTRIP:=:

include $(INCLUDE_DIR)/package.mk

define Package/rayhunter
  SECTION:=utils
  CATEGORY:=Utilities
  TITLE:=Rayhunter IMSI catcher detection for the GL.iNet Mudi V2
  DEPENDS:=@TARGET_ath79 +kmod-usb-serial-option
endef

define Package/rayhunter/description
  Reads Qualcomm diag from the modem's USB serial port and warns about
  signs of IMSI catchers in the web UI and on the OLED.
endef

define Package/rayhunter/conffiles
/etc/rayhunter/config.toml
endef

define Build/Compile
endef

define Package/rayhunter/install
	$(INSTALL_DIR) $(1)/usr/bin $(1)/etc/init.d $(1)/etc/rayhunter
	$(INSTALL_BIN) $(RAYHUNTER_BIN) $(1)/usr/bin/rayhunter-daemon
	$(INSTALL_BIN) ./files/rayhunter.init $(1)/etc/init.d/rayhunter
	$(INSTALL_CONF) ./files/config.toml $(1)/etc/rayhunter/config.toml
endef

define Package/rayhunter/postinst
#!/bin/sh
[ -n "$${IPKG_INSTROOT}" ] && exit 0
CONFIG=/etc/rayhunter/config.toml
# fresh install: keep recordings on the SD card if one is mounted, otherwise in RAM
if [ ! -e /etc/rayhunter/.installed ] && grep -q ' /mnt/sda1 ' /proc/mounts; then
	sed -i 's#^qmdl_store_path = .*#qmdl_store_path = "/mnt/sda1/rayhunter/qmdl"#' $$CONFIG
fi
touch /etc/rayhunter/.installed
case "$$(cat /etc/glversion 2>/dev/null)" in
	4.3.26|4.3.30) ;;
	*) echo "Note: Rayhunter has not been tested with GL firmware $$(cat /etc/glversion 2>/dev/null)." ;;
esac
found=
for d in /sys/bus/usb/devices/*; do
	[ "$$(cat $$d/idVendor 2>/dev/null):$$(cat $$d/idProduct 2>/dev/null)" = 2c7c:030b ] && found=1
done
[ -n "$$found" ] || echo "Note: no Quectel EM060K found (USB 2c7c:030b); only that modem has been tested."
echo "Recordings: $$(sed -n 's/^qmdl_store_path = //p' $$CONFIG). Change qmdl_store_path in $$CONFIG,"
echo "then run /etc/init.d/rayhunter restart. Web UI: http://192.168.8.1:8089/"
exit 0
endef

$(eval $(call BuildPackage,rayhunter))
