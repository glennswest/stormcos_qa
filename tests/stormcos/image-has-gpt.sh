#!/bin/sh
# QA-Name: image is a valid GPT disk
# QA-Desc: the built image has a protective MBR (0x55AA + 0xEE) at LBA0
# QA-Scope: image
# QA-Severity: blocking
# QA-Timeout: 60
#
# stormcos-owned. A boot-image with no protective MBR reads as raw data and
# will not boot (this exact bug bit us once) — cheap static guard.
set -eu
[ -f "$QA_IMAGE" ] || { echo "no image at QA_IMAGE=$QA_IMAGE"; exit 1; }
sig=$(dd if="$QA_IMAGE" bs=1 skip=510 count=2 2>/dev/null | od -An -tx1 | tr -d ' \n')
[ "$sig" = "55aa" ] || { echo "bad MBR boot signature: $sig"; exit 1; }
ptype=$(dd if="$QA_IMAGE" bs=1 skip=450 count=1 2>/dev/null | od -An -tx1 | tr -d ' \n')
[ "$ptype" = "ee" ] || { echo "not a GPT protective partition: type=$ptype"; exit 1; }
echo "GPT protective MBR present"
