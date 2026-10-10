// src/main.rs
// this is a Create a bootable USB tool.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{channel, Receiver};
use std::thread;

use eframe;
use egui;
use hadris_fs::sync::Volume;
use hadris_fs::{MountOptions, OpenOptions};
use hadris_iso::sync::IsoFs;
use hadris_storage::host::FileDevice;
use rfd;

#[derive(Clone, Debug)]
enum TaskResult {
    InProgress(String),
    Success(String),
    Error(String),
}

struct BootTool {
    iso_file_path: Option<String>,
    usb_device: String,
    task_receiver: Option<Receiver<TaskResult>>,
    task_status: Option<TaskResult>,
}

impl BootTool {
    fn new() -> Self {
        Self {
            iso_file_path: None,
            usb_device: String::new(),
            task_receiver: None,
            task_status: None,
        }
    }
}

fn show_message(title: &str, description: &str, level: rfd::MessageLevel) {
    rfd::MessageDialog::new()
        .set_title(title)
        .set_description(description)
        .set_level(level)
        .show();
}

fn ensure_root() -> Result<(), String> {
    let output = Command::new("id")
        .arg("-u")
        .output()
        .map_err(|err| format!("Failed to check current user: {}", err))?;

    let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if uid == "0" {
        Ok(())
    } else {
        Err("This tool must be run as root. Start it with: sudo ./target/debug/boot_tool".to_string())
    }
}

fn run_shell(args: &[&str], action: &str) -> Result<(), String> {
    if args.is_empty() {
        return Err(format!("No command to run for {action}"));
    }

    let mut command = Command::new(args[0]);
    command.args(&args[1..]);
    let status = command
        .status()
        .map_err(|err| format!("Failed to run {action}: {err}"))?;

    if !status.success() {
        return Err(format!("{action} failed with exit code {:?}", status.code()));
    }

    Ok(())
}

fn iso_path_for_partition(usb_device: &str, index: usize) -> String {
    if usb_device.contains("nvme") {
        format!("{}p{}", usb_device, index)
    } else {
        format!("{}{}", usb_device, index)
    }
}

fn copy_iso_tree<F>(volume: &Volume<F>, src_path: &str, dst_dir: &Path, skip_name: Option<&str>) -> Result<(), String>
where
    F: hadris_fs::sync::FileSystem,
{
    let metadata = volume
        .metadata(src_path)
        .map_err(|err| format!("Failed to stat {}: {}", src_path, err))?;

    if metadata.file_type().is_dir() {
        fs::create_dir_all(dst_dir)
            .map_err(|err| format!("Failed to create directory {}: {}", dst_dir.display(), err))?;

        for entry in volume.read_dir(src_path).map_err(|err| format!("Failed to read directory {}: {}", src_path, err))? {
            let entry = entry.map_err(|err| format!("Failed to read directory entry in {}: {}", src_path, err))?;
            let name = entry.name().to_string();

            if skip_name == Some(name.as_str()) {
                continue;
            }

            let child_src = if src_path == "/" {
                format!("/{}", name)
            } else {
                format!("{}/{}", src_path.trim_end_matches('/'), name)
            };

            let child_dst = dst_dir.join(name);
            copy_iso_tree(volume, &child_src, &child_dst.as_path(), None)?;
        }

        return Ok(());
    }

    let mut file = volume
        .open(src_path, OpenOptions::new().read())
        .map_err(|err| format!("Failed to open {}: {}", src_path, err))?;

    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|err| format!("Failed to read {}: {}", src_path, err))?;

    if let Some(parent) = dst_dir.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("Failed to create parent {}: {}", parent.display(), err))?;
    }

    fs::write(dst_dir, bytes)
        .map_err(|err| format!("Failed to write {}: {}", dst_dir.display(), err))?;

    Ok(())
}

fn create_bootable_usb(iso_file_path: &str, usb_device: &str) -> Result<(), String> {
    let iso_path = Path::new(iso_file_path);
    if !iso_path.exists() {
        return Err(format!("ISO file does not exist: {}", iso_file_path));
    }

    if usb_device.trim().is_empty() || !usb_device.starts_with("/dev/") {
        return Err("Select a USB device like /dev/sdX or /dev/nvme0n1".to_string());
    }

    let file = File::open(iso_path)
        .map_err(|err| format!("Failed to open ISO: {}", err))?;
    let iso_device = FileDevice::new(file)
        .map_err(|err| format!("Failed to wrap ISO as a block device: {}", err))?;
    let iso_fs = IsoFs::mount(iso_device, MountOptions::new())
        .map_err(|err| format!("Failed to mount ISO with Hadris: {}", err))?;
    let iso_volume = Volume::new(iso_fs);

    let mut has_efi = false;
    for entry in iso_volume
        .read_dir("/")
        .map_err(|err| format!("Failed to read ISO root: {}", err))?
    {
        let entry = entry.map_err(|err| format!("Failed to read ISO root entry: {}", err))?;
        let name = entry.name().to_string();
        if name.eq_ignore_ascii_case("EFI") {
            has_efi = true;
            break;
        }
    }

    if !has_efi {
        return Err("The ISO does not contain an EFI folder; it is not suitable for a UEFI bootable USB.".to_string());
    }

    let part1 = iso_path_for_partition(usb_device, 1);
    let part2 = iso_path_for_partition(usb_device, 2);
    let esp_mount = PathBuf::from("/mnt/boot_tool_esp");
    let ext4_mount = PathBuf::from("/mnt/boot_tool_root");

    run_shell(&["parted", "-s", usb_device, "mklabel", "gpt"], "create GPT partition table")?;
    run_shell(&["parted", "-s", usb_device, "mkpart", "ESP", "fat32", "1MiB", "513MiB"], "create ESP partition")?;
    run_shell(&["parted", "-s", usb_device, "set", "1", "esp", "on"], "mark ESP partition as bootable")?;
    run_shell(&["parted", "-s", usb_device, "mkpart", "root", "ext4", "513MiB", "100%"], "create root partition")?;
    run_shell(&["mkfs.vfat", "-F", "32", &part1], "format ESP partition")?;
    run_shell(&["mkfs.ext4", "-F", &part2], "format root partition")?;

    fs::create_dir_all(&esp_mount)
        .map_err(|err| format!("Failed to create ESP mount point: {}", err))?;
    fs::create_dir_all(&ext4_mount)
        .map_err(|err| format!("Failed to create root mount point: {}", err))?;

    run_shell(&["mount", &part1, esp_mount.to_str().unwrap()], "mount ESP partition")?;
    run_shell(&["mount", &part2, ext4_mount.to_str().unwrap()], "mount root partition")?;

    copy_iso_tree(&iso_volume, "/EFI", &esp_mount, None)
        .map_err(|err| format!("Failed to copy EFI files to the ESP partition: {}", err))?;
    copy_iso_tree(&iso_volume, "/", &ext4_mount, Some("EFI"))
        .map_err(|err| format!("Failed to copy ISO files to the root partition: {}", err))?;

    run_shell(&["umount", esp_mount.to_str().unwrap()], "unmount ESP partition")?;
    run_shell(&["umount", ext4_mount.to_str().unwrap()], "unmount root partition")?;

    Ok(())
}

impl eframe::App for BootTool {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Check for background task results
        if let Some(receiver) = &self.task_receiver {
            match receiver.try_recv() {
                Ok(result) => {
                    self.task_status = Some(result.clone());
                    match result {
                        TaskResult::Success(msg) => {
                            show_message("Success", &msg, rfd::MessageLevel::Info);
                            self.task_receiver = None;
                        }
                        TaskResult::Error(err) => {
                            show_message("Error", &err, rfd::MessageLevel::Error);
                            self.task_receiver = None;
                        }
                        TaskResult::InProgress(_) => {}
                    }
                }
                Err(_) => {}
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Boot Tool");
            ui.label("Select an ISO file to create a bootable USB:");

            if ui.button("Select ISO File").clicked() {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("ISO files", &["iso"])
                    .pick_file()
                {
                    self.iso_file_path = Some(path.display().to_string());
                }
            }

            if let Some(ref path) = self.iso_file_path {
                ui.label(format!("Selected ISO file: {}", path));
            }

            ui.add_space(12.0);
            ui.label("USB device:");
            ui.text_edit_singleline(&mut self.usb_device);
            ui.label("Example: /dev/sdX or /dev/nvme0n1");

            // Show status if task is running
            if let Some(TaskResult::InProgress(msg)) = &self.task_status {
                ui.label(egui::RichText::new(msg).color(egui::Color32::YELLOW));
                ui.spinner();
            }

            let button_enabled = self.task_receiver.is_none();
            if ui
                .add_enabled(button_enabled, egui::Button::new("Create bootable USB"))
                .clicked()
            {
                if let Some(path) = self.iso_file_path.as_deref() {
                    let iso_path = path.to_string();
                    let usb_dev = self.usb_device.clone();

                    // Create a channel for the background task
                    let (tx, rx) = channel();
                    self.task_receiver = Some(rx);
                    self.task_status = Some(TaskResult::InProgress("Starting USB creation...".to_string()));

                    // Spawn a background thread to avoid blocking the UI
                    thread::spawn(move || {
                        let result = create_bootable_usb(&iso_path, &usb_dev);
                        let task_result = match result {
                            Ok(()) => TaskResult::Success(format!(
                                "Bootable USB created successfully on {}",
                                usb_dev
                            )),
                            Err(err) => TaskResult::Error(err),
                        };
                        let _ = tx.send(task_result);
                    });
                } else {
                    show_message(
                        "Error",
                        "Please select an ISO file before creating the USB drive.",
                        rfd::MessageLevel::Error,
                    );
                }
            }
        });
    }
}

fn main() -> eframe::Result {
    if let Err(err) = ensure_root() {
        eprintln!("{}", err);
        std::process::exit(1);
    }

    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "Boot Tool",
        native_options,
        Box::new(|_cc| Ok(Box::new(BootTool::new()))),
    )
}
