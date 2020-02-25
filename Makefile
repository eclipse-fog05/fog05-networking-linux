# -*-Makefile-*-

WD := $(shell dirname $(realpath $(lastword $(MAKEFILE_LIST))));
UUID = $(shell ./to_uuid.sh)

LB_PLUGIN_DIR = /etc/fos/plugins/plugin-net-linuxbridge
LB_PLUGIN_CONFFILE = $(LB_PLUGIN_DIR)/linuxbridge_plugin.json
SYSTEMD_DIR = /lib/systemd/system/
BIN_DIR = /usr/bin

clean:
	echo "Nothing to do"
all:
	echo "Nothing to do"

install:
	sudo pip3 install jinja2 netifaces psutil
ifeq "$(wildcard $(LB_PLUGIN_DIR))" ""
	mkdir -p $(LB_PLUGIN_DIR)
	sudo cp -r ../plugin-net-linuxbridge/templates $(LB_PLUGIN_DIR)
	sudo cp ../plugin-net-linuxbridge/__init__.py $(LB_PLUGIN_DIR)
	sudo cp ../plugin-net-linuxbridge/linuxbridge_plugin $(LB_PLUGIN_DIR)
	sudo cp ../plugin-net-linuxbridge/README.md $(LB_PLUGIN_DIR)
	# sudo ln -sf /etc/fos/plugins/plugin-net-linuxbridge/linuxbridge_plugin /usr/bin/fos_linuxbridge
	sudo cp ../plugin-net-linuxbridge/get_face_address $(LB_PLUGIN_DIR)/get_face_address
	sudo ln -sf $(LB_PLUGIN_DIR)/get_face_address $(BIN_DIR)/fos_get_address
else
	sudo cp -r ../plugin-net-linuxbridge/templates $(LB_PLUGIN_DIR)
	sudo cp ../plugin-net-linuxbridge/__init__.py $(LB_PLUGIN_DIR)
	sudo cp ../plugin-net-linuxbridge/linuxbridge_plugin $(LB_PLUGIN_DIR)
	sudo cp ../plugin-net-linuxbridge/README.md $(LB_PLUGIN_DIR)
	# sudo ln -sf /etc/fos/plugins/plugin-net-linuxbridge/linuxbridge_plugin /usr/bin/fos_linuxbridge
	sudo cp ../plugin-net-linuxbridge/get_face_address $(LB_PLUGIN_DIR)/get_face_address
	sudo ln -sf $(LB_PLUGIN_DIR)/get_face_address $(BIN_DIR)/fos_get_address
endif
	sudo cp $(LB_PLUGIN_DIR)/fos_linuxbridge.service $(SYSTEMD_DIR)
	sudo sh -c "echo $(UUID) | xargs -i  jq  '.configuration.nodeid = \"{}\"' $(LB_PLUGIN_CONFFILE) > /tmp/linuxbridge_plugin.tmp && mv /tmp/linuxbridge_plugin.tmp $(LB_PLUGIN_CONFFILE)"


uninstall:
	sudo systemctl disable fos_linuxbridge
	sudo rm -rf $(LB_PLUGIN_DIR)
	sudo rm -rf /var/fos/linuxbridge
	sudo rm $(SYSTEMD_DIR)/fos_linuxbridge.service
	# sudo rm -rf /usr/bin/fos_linuxbridge
