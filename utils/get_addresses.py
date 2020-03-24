#!/usr/bin/env python3

# Copyright (c) 2014,2020 ADLINK Technology Inc.
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
# Contributors: Gabriele Baldoni, ADLINK Technology Inc. - LinuxBridge Plugin

import os
import psutil
import sys
import json
import socket
import ipaddress


def netmask_to_cidr(netmask):
    return sum([bin(int(x)).count('1') for x in netmask.split('.')])

def netmask6_to_cidr(netmask):
    return sum([bin(int(x,base=16)).count('1') for x in netmask.split(':')])

def main():

    result = {}

    ifs = psutil.net_if_addrs()
    for k in ifs:
        face_name = k
        v4 = ''
        v4mask = ''
        v6 = ''
        v6mask = ''
        mac = ''
        face_info = ifs[k]
        for i in face_info:
            if i[0] == socket.AddressFamily.AF_INET:
                v4 = i[1]
                v4mask = i[2]
            elif i[0] == socket.AddressFamily.AF_INET6:
                v6 = i[1].split('%')[0]
                v6mask = i[2]
            elif i[0] == socket.AddressFamily.AF_PACKET:
                mac = i[1]

        if v4 != '' and v4mask != '':
            bits = netmask_to_cidr(v4mask)
            v4 = '{}/{}'.format(v4,bits)

        if v6 != '' and v6mask != '':
            mask = ipaddress.IPv6Address(v6mask)
            bits = netmask6_to_cidr(mask.exploded)
            v6 = '{}/{}'.format(v6,bits)

        result.update({face_name:{'ipv4':v4,'ipv6':v6,'mac':mac}})

    print(json.dumps(result))


if __name__ == '__main__':
    main()