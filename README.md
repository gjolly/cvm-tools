# cvm-tools

Setup and run TPM backed FDE VMs with qemu.

## Pre-requisite

Install the following packages:

```bash
apt install -y \
    cloud-image-utils \
    qemu-utils \
    qemu-system-x86 \
    swtpm \
    tpm2-tools \
    python3-swiftclient
```

## Usage

### Example

```bash
# download the latest VM image
cvm-tools image download

# customize it to remove make the VM
# configurable outside of Azure
cvm-tools image customize

# setup a vTPM locally and create SRK
cvm-tools tpm setup

# start the vTMP
cvm-tools tpm setup

# encrypt and deploy the VM using
# github.com/canonical/encrypt-cloud-image
# ...encrypt
encrypt-cloud-image/encrypt-cloud-image encrypt ./livecd.ubuntu-cpc.azure.fde.vhd -o jammy-encrypted.vhd
# ...deploy using SRK and uefi.json
encrypt-cloud-image deploy \
    --srk-pub ./srk.pub \
    --uefi-config ./uefi.json \
    --add-efi-boot-manager-profile \
    --add-efi-secure-boot-profile \
    --add-ubuntu-kernel-profile \
    ./jammy-encrypted.vhd

# start the VM
cvm-tools vm run --image ./jammy-encrypted.vhd

# kill the VM
cvm-tools vm kill
```

### Disk image management

```bash
# download image from swift
cvm-tools image download

# - disable walinuxagent
# - setup NoCloud datasource for cloud-init
cvm-tools image customize
```

### vTPM management

```bash
# Setup vTPM (generate SRK)
cvm-tools tpm setup

# Start vTPM
cvm-tools tpm start

# Kill vTPM
cvm-tools tpm kill

# Destroy vTPM state
cvm-tools tpm destroy
```

### VM management

```bash
# start VM
cvm-tools vm start [--image IMAGE]

# stop VM
cvm-tools vm kill
```

## Build

```bash
cargo build
```
