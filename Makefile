# -*-Makefile-*-

WD := $(shell dirname $(realpath $(lastword $(MAKEFILE_LIST))));
UUID = $(shell ./to_uuid.sh)

LB_PLUGIN_DIR = /etc/fos/plugins/plugin-net-linuxbridge
LB_PLUGIN_CONFFILE = /etc/fos/plugins/plugin-net-linuxbridge/linuxbridge_plugin.json
all:
	echo "Nothing to do"

install:
	sudo pip3 install jinja2 netifaces psutil
ifeq "$(wildcard $(LB_PLUGIN_DIR))" ""
	sudo cp -r ../plugin-net-linuxbridge /etc/fos/plugins/
else
	sudo cp -r ../plugin-net-linuxbridge/templates /etc/fos/plugins/plugin-net-linuxbridge/
	sudo cp ../plugin-net-linuxbridge/__init__.py /etc/fos/plugins/plugin-net-linuxbridge/
	sudo cp ../plugin-net-linuxbridge/linuxbridge_plugin /etc/fos/plugins/plugin-net-linuxbridge/
	sudo cp ../plugin-net-linuxbridge/README.md /etc/fos/plugins/plugin-net-linuxbridge/
	sudo ln -sf /etc/fos/plugins/plugin-net-linuxbridge/linuxbridge_plugin /usr/bin/fos_linuxbridge
	sudo cp ../plugin-net-linuxbridge/get_face_address /etc/fos/plugins/plugin-net-linuxbridge/get_face_address
	sudo ln -sf /etc/fos/plugins/plugin-net-linuxbridge/get_face_address /usr/bin/fos_get_address
endif
	sudo cp /etc/fos/plugins/plugin-net-linuxbridge/fos_linuxbridge.service /lib/systemd/system/
	sudo sh -c "echo $(UUID) | xargs -i  jq  '.configuration.nodeid = \"{}\"' /etc/fos/plugins/plugin-net-linuxbridge/linuxbridge_plugin.json > /tmp/linuxbridge_plugin.tmp && mv /tmp/linuxbridge_plugin.tmp /etc/fos/plugins/plugin-net-linuxbridge/linuxbridge_plugin.json"


uninstall:
	sudo systemctl disable fos_linuxbridge
	sudo rm -rf /etc/fos/plugins/plugin-net-linuxbridge
	sudo rm -rf /var/fos/linuxbridge
	sudo rm /lib/systemd/system/fos_linuxbridge.service
	sudo rm -rf /usr/bin/fos_linuxbridge
