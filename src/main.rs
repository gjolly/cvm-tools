use anyhow::{anyhow, Result};
use clap;
use clap::arg;
use std::fs;
use std::process::Command;
use std::{env, thread, time};
use std::{io::Write, path::Path};

#[derive(PartialEq, Default, Clone, Debug)]
struct Commit {
    hash: String,
    message: String,
}

fn download_image(suite: &str, serial: &str, file: &str, force: bool) -> Result<()> {
    if !force && Path::new(file).exists() {
        return Ok(());
    }

    let object = format!("fde-server/{suite}/{serial}/private/{file}");
    let container = "cloud-images";

    let output = Command::new("swift")
        .arg("download")
        .arg("--output")
        .arg(file)
        .arg(container)
        .arg(object)
        .output()?;

    if !output.status.success() {
        let err = String::from_utf8(output.stderr)?;
        return Err(anyhow!(err));
    }

    Ok(())
}

fn extract_archive(archive: &str) -> Result<()> {
    let output = Command::new("tar").arg("xvf").arg(archive).output()?;

    if !output.status.success() {
        let err = String::from_utf8(output.stderr)?;
        return Err(anyhow!(err));
    }

    Ok(())
}

fn customize_cloudinit(mountpoint: &str) -> Result<()> {
    // add NoCloud to cloud-init datasource
    let mut file = fs::File::create(format!("{mountpoint}/etc/cloud/cloud.cfg.d/90_dpkg.cfg"))?;
    file.write_all(b"datasource_list: [ NoCloud, Azure ]\n")?;

    Ok(())
}

fn attach_nbd_device(nbd_device: &str, image: &str) -> Result<()> {
    // attach the image to a nbd chardev
    let output = Command::new("qemu-nbd")
        .arg("--format")
        .arg("vpc")
        .arg(format!("--connect={nbd_device}"))
        .arg(image)
        .output()?;

    if !output.status.success() {
        let err = String::from_utf8(output.stderr)?;
        return Err(anyhow!(err));
    }

    // nbd is async so we want to make sure the device is attached
    // before returning otherwise mount will fail
    let delay = time::Duration::from_millis(50);
    let mut retries = 0;
    let partition = format!("{nbd_device}p1");
    while !Path::new(&partition).exists() && retries < 20 {
        thread::sleep(delay);
        retries += 1;
    }

    if !Path::new(&partition).exists() {
        return Err(anyhow!("nbd device not created"));
    }

    Ok(())
}

fn customize_rootfs(mountpoint: &str) -> Result<()> {
    // disable walinuxagent
    let output = Command::new("chroot")
        .arg(&mountpoint)
        .arg("systemctl")
        .arg("mask")
        .arg("walinuxagent")
        .output()?;

    if !output.status.success() {
        let err = String::from_utf8(output.stderr)?;
        return Err(anyhow!(err));
    }

    customize_cloudinit(&mountpoint)?;

    Ok(())
}

fn customize_image(image: &str) -> Result<()> {
    // make sure the nbd module is loaded
    let output = Command::new("modprobe").arg("nbd").output()?;

    if !output.status.success() {
        let err = String::from_utf8(output.stderr)?;
        return Err(anyhow!(err));
    }

    let nbd_device = "/dev/nbd0";

    attach_nbd_device(nbd_device, image)?;

    // mount the rootfs
    let mountpoint = format!("{}/mountpoint", env::temp_dir().display());
    fs::create_dir_all(&mountpoint)?;

    let output = Command::new("mount")
        .arg(format!("{nbd_device}p1"))
        .arg(&mountpoint)
        .output()?;
    if !output.status.success() {
        let err = String::from_utf8(output.stderr)?;
        return Err(anyhow!(err));
    }

    customize_rootfs(&mountpoint)?;

    // umounting
    let output = Command::new("umount").arg(&mountpoint).output()?;

    if !output.status.success() {
        let err = String::from_utf8(output.stderr)?;
        return Err(anyhow!(err));
    }

    // removing mountpoint
    fs::remove_dir(&mountpoint)?;

    // disconnecting nbd
    let output = Command::new("qemu-nbd")
        .arg("--disconnect")
        .arg(nbd_device)
        .output()?;

    if !output.status.success() {
        let err = String::from_utf8(output.stderr)?;
        return Err(anyhow!(err));
    }

    Ok(())
}

fn create_cloudinit_drive(key_id: &str) -> Result<String> {
    let drive = String::from("seed.img");

    let user_data = format!("{}/user_data.yaml", env::temp_dir().display());
    let mut file = fs::File::create(&user_data)?;

    writeln!(&mut file, "#cloud-config")?;
    writeln!(&mut file, "ssh_import_id:")?;
    writeln!(&mut file, "  - {}", key_id)?;

    let output = Command::new("cloud-localds")
        .arg(&drive)
        .arg(&user_data)
        .output()?;

    if !output.status.success() {
        let err = String::from_utf8(output.stderr)?;
        return Err(anyhow!(err));
    }

    Ok(drive)
}

fn start_vm(image: &str, cloudinit_drive: &str, vtpm_socket: &str) -> Result<()> {
    let output = Command::new("qemu-system-x86_64")
        // basic VM config
        .arg("--cpu")
        .arg("host")
        .arg("-machine")
        .arg("type=q35,accel=kvm")
        .arg("-m")
        .arg("2048")
        // config for qemu process
        .arg("-daemonize")
        .arg("-pidfile")
        .arg("/tmp/qemu_pid")
        .arg("-qmp")
        .arg("unix:/tmp/qemu-qmp.sock,server=on,wait=off")
        // Run the VM without modifying attached disks
        .arg("-snapshot")
        // Configuring networking
        .arg("-netdev")
        .arg("id=net00,type=user,hostfwd=tcp::2222-:22")
        .arg("-device")
        .arg("virtio-net-pci,netdev=net00")
        // tpm
        .arg("-chardev")
        .arg(format!("socket,id=chrtpm,path={vtpm_socket}"))
        .arg("-tpmdev")
        .arg("emulator,id=tpm0,chardev=chrtpm")
        .arg("-device")
        .arg("tpm-tis,tpmdev=tpm0")
        // Attaching image drive
        .arg("-drive")
        .arg(format!("if=virtio,format=vpc,file={image}"))
        // Attaching cloud-init drive (for NoCloud datasource)
        .arg("-drive")
        .arg(format!("if=virtio,format=raw,file={cloudinit_drive}"))
        // Attaching OVMF firwmware code for UEFI boot
        .arg("-drive")
        .arg("if=pflash,format=raw,unit=0,file=/usr/share/OVMF/OVMF_CODE.fd,readonly=on")
        // Running the command
        .output()?;

    if !output.status.success() {
        let err = String::from_utf8(output.stderr)?;
        return Err(anyhow!(err));
    }

    Ok(())
}

fn start_vtpm() -> Result<String> {
    let tpm_directory = "/tmp/vtpm";
    let tpm_socket = String::from(format!("{tpm_directory}/swtpm-sock"));

    fs::create_dir_all(tpm_directory)?;

    let output = Command::new("swtpm")
        .arg("socket")
        .arg("--tpm2")
        .arg("--pid")
        .arg("file=/tmp/swtpm_pid")
        .arg("--tpmstate")
        .arg(format!("dir={tpm_directory}"))
        .arg("--ctrl")
        .arg(format!("type=unixio,path={tpm_socket}"))
        .arg("-d")
        .output()?;

    if !output.status.success() {
        let err = String::from_utf8(output.stderr)?;
        return Err(anyhow!(err));
    }

    Ok(tpm_socket)
}

/// Customize and start a CVM image
fn kill_process(pid_file: &str) -> Result<()> {
    let pid = fs::read_to_string(pid_file)?;

    let output = Command::new("kill").arg(pid.trim()).output()?;

    if !output.status.success() {
        let err = String::from_utf8(output.stderr)?;
        return Err(anyhow!(err));
    }

    Ok(())
}

fn kill_vm() -> Result<()> {
    let pid_file = "/tmp/qemu_pid";
    kill_process(pid_file)?;
    fs::remove_file(pid_file)?;

    Ok(())
}

fn kill_vtpm() -> Result<()> {
    let pid_file = "/tmp/swtpm_pid";
    kill_process(pid_file)?;
    fs::remove_file(pid_file)?;

    Ok(())
}

fn cli() -> clap::Command {
    clap::Command::new("cvm-tools")
        .about("A tool for managing vTPM backed FDE images and VMs.")
        .subcommand_required(true)
        .subcommand(
            clap::Command::new("image")
                .about("Manage cloud images")
                .subcommand_required(true)
                .subcommand(
                    clap::Command::new("download").arg(
                        arg!(--suite[SUITE])
                            .default_value("jammy")
                            .default_missing_value("always"),
                    ),
                )
                .subcommand(clap::Command::new("customize")),
        )
        .subcommand(
            clap::Command::new("vm")
                .about("Manage VMs")
                .subcommand_required(true)
                .subcommand(clap::Command::new("run"))
                .subcommand(clap::Command::new("kill")),
        )
}

fn main() -> Result<()> {
    let matches = cli().get_matches();

    let key_id = "gh:gjolly";
    let image = "livecd.ubuntu-cpc.azure.fde.vhd";

    match matches.subcommand() {
        Some(("image", sub_matches)) => match sub_matches.subcommand() {
            Some(("download", ssub_matches)) => {
                let suite = ssub_matches.get_one::<String>("suite").expect("required");

                let image_archive = format!("{suite}-server-cloudimg-amd64-azure.fde.vhd.tar.gz");

                println!("Downloading image file from swift: {}", &image_archive);
                download_image(suite, "20231128", &image_archive, false)?;

                println!("Extracting archive: {}", &image_archive);
                extract_archive(&image_archive)?;
            }
            Some(("customize", _)) => {
                println!("Customizing image: {}", &image);
                customize_image(&image)?;
            }
            _ => {
                println!("not implemented");
            }
        },
        Some(("vm", sub_matches)) => match sub_matches.subcommand() {
            Some(("run", _)) => {
                println!("Staring vTPM");
                let tpm_socket = start_vtpm()?;

                println!("Creating cloud-init config drive: {}", &image);
                let cloudinit_drive = create_cloudinit_drive(key_id)?;

                println!("Starting VM");
                start_vm(&image, &cloudinit_drive, &tpm_socket)?;
                println!("VM started, to kill run:");
                println!("    kill $(cat /tmp/qemu_pid)");
                println!("connect to QMP with:");
                println!("    qmp-shell /tmp/qemu-qmp.sock");
            }
            Some(("kill", _)) => {
                let _ = kill_vm();
                let _ = kill_vtpm();
            }
            _ => {
                println!("not implemented");
            }
        },
        _ => {
            println!("not implemented");
        }
    }

    Ok(())
}
