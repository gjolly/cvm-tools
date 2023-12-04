use anyhow::{anyhow, Result};
use clap;
use clap::arg;
use indicatif::{ProgressBar, ProgressStyle};
use std::fs;
use std::io;
use std::io::Read;
use std::process::Command;
use std::{env, thread, time};
use std::{io::Write, path::Path};
use ureq;

#[derive(PartialEq, Default, Clone, Debug)]
struct Commit {
    hash: String,
    message: String,
}

fn azure_create_group(group_name: &str) -> Result<()> {
    let location = "northeurope";

    let output = Command::new("az")
        .arg("group")
        .arg("create")
        .arg("--location")
        .arg(location)
        .arg("--resource-group")
        .arg(group_name)
        .output()?;

    if !output.status.success() {
        let err = String::from_utf8(output.stderr)?;
        return Err(anyhow!(err));
    }

    Ok(())
}

fn azure_create_disk(group_name: &str, disk_name: &str, urn: &str) -> Result<()> {
    let output = Command::new("az")
        .arg("disk")
        .arg("create")
        .arg("--resource-group")
        .arg(group_name)
        .arg("--name")
        .arg(disk_name)
        .arg("--hyper-v-generation")
        .arg("V2")
        .arg("--image-reference")
        .arg(urn)
        .output()?;

    if !output.status.success() {
        let err = String::from_utf8(output.stderr)?;
        return Err(anyhow!(err));
    }

    Ok(())
}

fn azure_export_disk(group_name: &str, disk_name: &str) -> Result<String> {
    let output = Command::new("az")
        .arg("disk")
        .arg("grant-access")
        .arg("--resource-group")
        .arg(group_name)
        .arg("--name")
        .arg(disk_name)
        .arg("--duration")
        // 24h
        .arg("86400")
        .arg("--query")
        .arg("accessSas")
        .output()?;

    if !output.status.success() {
        let err = String::from_utf8(output.stderr)?;
        return Err(anyhow!(err));
    }
    let url_with_quotes = String::from_utf8(output.stdout)?;
    let url = url_with_quotes.trim().trim_matches('"');

    Ok(url.to_string())
}

struct SizeLimitedReader<R> {
    inner: R,
    remaining_bytes: usize,
    progress_bar: ProgressBar,
}

impl<R: Read> SizeLimitedReader<R> {
    pub fn new(inner: R, size: usize) -> Self {
        let progress_bar = ProgressBar::new(size as u64);
        progress_bar.set_style(
            ProgressStyle::default_bar()
                .template("[{elapsed_precise}] {bar:40.cyan/blue} {bytes}/{total_bytes} ({eta})")
                .unwrap()
                .progress_chars("##-"),
        );

        SizeLimitedReader {
            inner,
            remaining_bytes: size,
            progress_bar,
        }
    }
}

impl<R: Read> Read for SizeLimitedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining_bytes == 0 {
            return Ok(0); // Already read the specified number of bytes
        }

        let bytes_to_read = buf.len().min(self.remaining_bytes);
        let bytes_read = self.inner.read(&mut buf[..bytes_to_read])?;
        self.remaining_bytes -= bytes_read;

        self.progress_bar.inc(bytes_read as u64);

        if self.remaining_bytes == 0 {
            self.progress_bar.finish_and_clear();
        }

        Ok(bytes_read)
    }
}

fn azure_download_disk(url: &str, filename: &str) -> Result<()> {
    let mut file = fs::File::create(filename)?;

    let mut body = ureq::get(url).call()?.into_reader();
    let mut reader = SizeLimitedReader::new(&mut body, 4 * usize::pow(1024, 3));

    let mut retries = 0;

    while retries < 10 {
        match io::copy(&mut reader, &mut file) {
            Ok(_) => break,
            Err(err) => {
                println!("{:?}", err);
            }
        };

        retries += 1;
    }

    Ok(())
}

fn azure_delete_group(group_name: &str) -> Result<()> {
    // az group delete --no-wait -y -g
    let output = Command::new("az")
        .arg("group")
        .arg("delete")
        .arg("--resource-group")
        .arg(group_name)
        .arg("--no-wait")
        .arg("--yes")
        .output()?;

    if !output.status.success() {
        let err = String::from_utf8(output.stderr)?;
        return Err(anyhow!(err));
    }

    Ok(())
}

fn download_image(suite: &str, file: &str, force: bool) -> Result<()> {
    if !force && Path::new(file).exists() {
        return Ok(());
    }

    let group_name = "cvm-tools-rg4";
    let disk_name = "cvm-tools-disk";
    let urn = format!("Canonical:0001-com-ubuntu-confidential-vm-{suite}:22_04-lts-cvm:latest");

    azure_create_group(group_name)?;
    azure_create_disk(group_name, disk_name, &urn)?;
    let url = azure_export_disk(group_name, disk_name)?;

    println!("downloading disk, may take a while...");
    azure_download_disk(&url, file)?;

    azure_delete_group(group_name)?;

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
        .arg("raw")
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
    let mut file = match fs::File::create(&user_data) {
        Ok(file) => file,
        Err(err) => return Err(anyhow!(format!("failed to create user_data.yaml {}", err))),
    };

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

fn copy_ovmf_vars() -> Result<String> {
    let copy_path = String::from("/tmp/OVMF_VARS.ms.fd");
    fs::copy("/usr/share/OVMF/OVMF_VARS_4M.ms.fd", &copy_path)?;

    Ok(copy_path)
}

fn start_vm(image: &str, cloudinit_drive: &str, vtpm_socket: &str) -> Result<()> {
    let mut cmd = Command::new("qemu-system-x86_64");

    let ovmf_vars = match copy_ovmf_vars() {
        Ok(path) => path,
        Err(err) => {
            return Err(anyhow!(format!("failed to copy OVMF: {:?}", err)));
        }
    };

    // basic VM config
    cmd.arg("--cpu")
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
        .arg(format!("socket,id=chrtpm,path={vtpm_socket}.ctrl"))
        .arg("-tpmdev")
        .arg("emulator,id=tpm0,chardev=chrtpm")
        .arg("-device")
        .arg("tpm-tis,tpmdev=tpm0")
        // Attaching image drive
        .arg("-drive")
        .arg(format!("if=virtio,format=raw,file={image}"))
        // Attaching cloud-init drive (for NoCloud datasource)
        .arg("-drive")
        .arg(format!("if=virtio,format=raw,file={cloudinit_drive}"))
        // Attaching OVMF firwmware code for UEFI boot
        .arg("-drive")
        .arg("if=pflash,format=raw,unit=0,file=/usr/share/OVMF/OVMF_CODE_4M.ms.fd,readonly=on")
        .arg("-drive")
        .arg(format!("if=pflash,format=raw,unit=1,file={ovmf_vars}"));

    // Running the command
    let output = match cmd.output() {
        Ok(output) => output,
        Err(err) => {
            return Err(anyhow!(format!("failed to run qemu: {:?}", err)));
        }
    };

    if !output.status.success() {
        return Err(anyhow!(String::from_utf8(output.stderr)?));
    }

    Ok(())
}

fn start_vtpm(state_directory: &str, socket: &str, pid_file: &str, server: bool) -> Result<()> {
    fs::create_dir_all(state_directory)?;

    let mut cmd = Command::new("swtpm");
    cmd.arg("socket")
        .arg("--tpm2")
        .arg("--pid")
        .arg(format!("file={}", pid_file))
        .arg("--tpmstate")
        .arg(format!("dir={state_directory}"))
        .arg("--ctrl")
        .arg(format!("type=unixio,path={socket}.ctrl"))
        .arg("--flags")
        .arg("not-need-init,startup-clear")
        .arg("-d");

    if server {
        cmd.arg("--server")
            .arg(format!("type=unixio,path={socket}"));
    }

    let output = cmd.output()?;
    if !output.status.success() {
        let err = String::from_utf8(output.stderr)?;
        return Err(anyhow!(err));
    }

    Ok(())
}

fn status_vtpm(state_directory: &str, pid_file: &str) -> String {
    match fs::read_to_string(pid_file) {
        Ok(pid) => return format!("vTPM is running, pid: {}", pid),
        Err(_) => {
            if Path::new(state_directory).join("tpm2-00.permall").exists() {
                return format!("vTPM setup but not running");
            };

            return "vTPM not setup and not running".to_string();
        }
    };
}

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

fn destroy_vtpm(directory: &str) -> Result<()> {
    fs::remove_dir_all(directory)?;

    Ok(())
}

fn generate_srk(socket: &str) -> Result<()> {
    let output = Command::new("tpm2_createprimary")
        .arg("-T")
        .arg(format!("swtpm:path={socket}"))
        .arg("-c")
        .arg("srk.ctx")
        .output()?;

    if !output.status.success() {
        let err = String::from_utf8(output.stderr)?;
        return Err(anyhow!(err));
    }

    let output = Command::new("tpm2_readpublic")
        .arg("-T")
        .arg(format!("swtpm:path={socket}"))
        .arg("-c")
        .arg("srk.ctx")
        .arg("-o")
        .arg("srk.pub")
        .output()?;

    if !output.status.success() {
        let err = String::from_utf8(output.stderr)?;
        return Err(anyhow!(err));
    }

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
                        arg!(--suite <SUITE>)
                            .default_value("jammy")
                            .default_missing_value("always"),
                    ),
                )
                .subcommand(
                    clap::Command::new("customize").arg(
                        arg!([IMAGE])
                    ),
                ),
        )
        .subcommand(
            clap::Command::new("tpm")
                .about("Manage vTPM")
                .subcommand_required(true)
                .subcommand(clap::Command::new("start"))
                .subcommand(clap::Command::new("setup"))
                .subcommand(clap::Command::new("kill"))
                .subcommand(clap::Command::new("destroy"))
                .subcommand(clap::Command::new("status")),
        )
        .subcommand(
            clap::Command::new("vm")
                .about("Manage VMs")
                .subcommand_required(true)
                .subcommand(
                    clap::Command::new("start").arg(
                        arg!([IMAGE])
                    ),
                )
                .subcommand(clap::Command::new("kill")),
        )
}

fn check_dependencies(dependencies: Vec<&str>) -> Result<()> {
    for dep in dependencies {
        let output = Command::new("which").arg(&dep).output()?;
        if !output.status.success() {
            return Err(anyhow!(format!("{} not installed", &dep)));
        }
    }

    Ok(())
}

fn main() -> Result<()> {
    let matches = cli().get_matches();

    let key_id = "gh:gjolly";

    let tpm_pid_file = "/tmp/vtpm_pid";
    let tpm_directory = "/tmp/vtpm";
    let tpm_socket = String::from(format!("{tpm_directory}/swtpm-sock"));

    match matches.subcommand() {
        Some(("image", sub_matches)) => match sub_matches.subcommand() {
            Some(("download", ssub_matches)) => {
                check_dependencies(vec!["az"])?;

                let suite = ssub_matches.get_one::<String>("SUITE").expect("required");

                let image_file = format!("{suite}.img");

                println!("Downloading image file from azure: {}", &image_file);
                download_image(suite, &image_file, false)?;
            }
            Some(("customize", ssub_matches)) => {
                check_dependencies(vec!["qemu-nbd"])?;
                let image = ssub_matches.get_one::<String>("IMAGE").expect("required");

                println!("Customizing image: {}", &image);
                customize_image(&image)?;
            }
            _ => {
                println!("not implemented");
            }
        },
        Some(("tpm", sub_matches)) => match sub_matches.subcommand() {
            Some(("start", _)) => {
                check_dependencies(vec!["swtpm"])?;

                println!("Staring vTPM");
                start_vtpm(&tpm_directory, &tpm_socket, &tpm_pid_file, false)?;
            }
            Some(("setup", _)) => {
                check_dependencies(vec!["swtpm", "tpm2"])?;

                println!("Creating SRK");
                start_vtpm(&tpm_directory, &tpm_socket, &tpm_pid_file, true)?;

                // TODO: verify that TPM socket exists
                generate_srk(&tpm_socket)?;

                kill_process(&tpm_pid_file)?;
            }
            Some(("kill", _)) => {
                println!("Stopping TPM");
                // TODO: verify that pid file exists
                kill_process(&tpm_pid_file)?;
            }
            Some(("destroy", _)) => {
                println!("Destroying vTPM state");
                // TODO: verify that pid file exists
                let _ = kill_process(&tpm_pid_file);
                destroy_vtpm(&tpm_directory)?;
            }
            Some(("status", _)) => {
                println!("{}", status_vtpm(&tpm_directory, &tpm_pid_file));
            }
            _ => {
                println!("not implemented");
            }
        },
        Some(("vm", sub_matches)) => {
            match sub_matches.subcommand() {
                Some(("start", ssub_matches)) => {
                    check_dependencies(vec!["qemu-system-x86_64", "cloud-localds"])?;

                    let image = ssub_matches.get_one::<String>("image").expect("required");

                    println!("Creating cloud-init config drive");
                    let cloudinit_drive = match create_cloudinit_drive(key_id) {
                        Ok(path) => path,
                        Err(err) => {
                            return Err(anyhow!(format!(
                                "failed to create cloud-init drive: {}",
                                err
                            )))
                        }
                    };

                    println!("Starting VM: {}", &image);
                    // TODO: verify that TPM socket exists
                    start_vm(&image, &cloudinit_drive, &tpm_socket)?;
                    println!("connect to QMP with:");
                    println!("    qmp-shell /tmp/qemu-qmp.sock");
                }
                Some(("kill", _)) => {
                    // TODO: verify that pid file exists
                    kill_vm()?;
                }
                _ => {
                    println!("not implemented");
                }
            }
        }
        _ => {
            println!("not implemented");
        }
    }

    Ok(())
}
