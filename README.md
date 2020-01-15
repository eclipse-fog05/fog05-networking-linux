<!--
# Copyright (c) 2014,2018 ADLINK Technology Inc.
#
# See the NOTICE file(s) distributed with this work for additional
# information regarding copyright ownership.
#
# This program and the accompanying materials are made available under the
# terms of the Eclipse Public License 2.0 which is available at
# http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
# which is available at https://www.apache.org/licenses/LICENSE-2.0.
#
# SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
#
# Contributors: Gabriele Baldoni, ADLINK Technology Inc. - Base plugins set

 -->


[![Gitter](https://badges.gitter.im/atolab/fog05.svg)](https://gitter.im/atolab/fog05?utm_source=badge&utm_medium=badge&utm_campaign=pr-badge)
[![License](https://img.shields.io/badge/License-EPL%202.0-blue)](https://choosealicense.com/licenses/epl-2.0/)
[![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)
[![Build Status](https://travis-ci.com/eclipse-fog05/plugin-net-linuxbridge.svg?branch=master)](https://travis-ci.com/eclipse-fog05/plugin-net-linuxbridge)


Linux Bridge Network Manager Plugin

This plugin allow fog05 to manage networks with bridge utils

supported operation:
- create virtual bridge
- create virtual network
- add interface to network
- delete virtual interface
- delete virtual bridge
- delete virtual network

todo:

- create virtual interface
- remove interface from network


---
package dependencies:

- bridge-utils
---


config dependencies:
- update the nodeid (result of `cat /etc/machine-id` ) in linuxbridge_plugin.json->configuration->nodeid, and in case the yaks server is not in the same machine, also linuxbridge_plugin.json->configuration->nodeid with the correct ip:port of the yaks server )
- user should be able to use sudo without password asking (`echo "username  ALL=(ALL) NOPASSWD: ALL"  >> /etc/sudoers`)
