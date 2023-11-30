# cvm-tools

Setup and run TPM backed FDE VMs with qemu.

## Usage

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
