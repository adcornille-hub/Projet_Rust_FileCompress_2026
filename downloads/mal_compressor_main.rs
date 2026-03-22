use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use colored::*;
use crc32fast::Hasher;
use flate2::write::{DeflateEncoder, DeflateDecoder};
use flate2::Compression;
use indicatif::{ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{Read, Write, BufReader, BufWriter};
use std::path::PathBuf;
use walkdir::WalkDir;

// ============================================================================
// STRUCTURES DE DONNÉES
// ============================================================================

const MAGIC: &[u8; 4] = b"MAL\0";
const VERSION: u8 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct MalHeader {
    magic: [u8; 4],
    version: u8,
    compression_level: u8,
    file_count: u32,
    total_original_size: u64,
    total_compressed_size: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct FileEntry {
    path: String,
    original_size: u64,
    compressed_size: u64,
    crc32: u32,
}

// ============================================================================
// CLI
// ============================================================================

#[derive(Parser)]
#[command(name = "mal")]
#[command(about = "Compresseur de fichiers MAL (Multi-Archive Lossless)", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Compresse des fichiers dans une archive .mal
    Compress {
        /// Fichiers ou dossiers à compresser
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
        
        /// Fichier de sortie (.mal)
        #[arg(short, long)]
        output: PathBuf,
        
        /// Niveau de compression (0=aucun, 1=rapide, 6=défaut, 9=maximum)
        #[arg(short, long, default_value = "6")]
        level: u8,
    },
    
    /// Décompresse une archive .mal
    Decompress {
        /// Fichier .mal à décompresser
        input: PathBuf,
        
        /// Dossier de destination
        #[arg(short, long, default_value = ".")]
        output: PathBuf,
    },
    
    /// Affiche le contenu d'une archive .mal
    List {
        /// Fichier .mal à inspecter
        input: PathBuf,
    },
    
    /// Vérifie l'intégrité d'une archive .mal
    Verify {
        /// Fichier .mal à vérifier
        input: PathBuf,
    },
}

// ============================================================================
// COMPRESSION
// ============================================================================

fn collect_files(inputs: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    
    for input in inputs {
        if input.is_file() {
            files.push(input.clone());
        } else if input.is_dir() {
            for entry in WalkDir::new(input).follow_links(false) {
                let entry = entry?;
                if entry.file_type().is_file() {
                    files.push(entry.path().to_path_buf());
                }
            }
        }
    }
    
    Ok(files)
}

fn compress_files(inputs: Vec<PathBuf>, output: PathBuf, level: u8) -> Result<()> {
    if level > 9 {
        anyhow::bail!("Le niveau de compression doit être entre 0 et 9");
    }
    
    println!("{}", "📦 Collecte des fichiers...".bright_cyan().bold());
    let files = collect_files(&inputs)?;
    
    if files.is_empty() {
        anyhow::bail!("Aucun fichier à compresser");
    }
    
    println!("   {} fichier(s) trouvé(s)\n", files.len());
    
    // Calculer la taille totale
    let total_size: u64 = files.iter()
        .filter_map(|f| fs::metadata(f).ok())
        .map(|m| m.len())
        .sum();
    
    let compression = match level {
        0 => Compression::none(),
        1 => Compression::fast(),
        9 => Compression::best(),
        _ => Compression::new(level as u32),
    };
    
    // Barre de progression globale
    let pb = ProgressBar::new(total_size);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({eta})")
            .unwrap()
            .progress_chars("#>-")
    );
    
    println!("{}", "🗜️  Compression en cours...".bright_green().bold());
    
    let mut file_entries = Vec::new();
    let mut total_compressed = 0u64;
    let mut all_compressed_data = Vec::new();
    
    // Compresser chaque fichier
    for file_path in &files {
        let file_name = file_path.to_string_lossy().to_string();
        
        // Lire le fichier
        let mut file = File::open(file_path)
            .context(format!("Impossible d'ouvrir {}", file_name))?;
        let mut file_data = Vec::new();
        file.read_to_end(&mut file_data)?;
        
        let original_size = file_data.len() as u64;
        
        // Calculer CRC32
        let mut hasher = Hasher::new();
        hasher.update(&file_data);
        let crc32 = hasher.finalize();
        
        // Compresser
        let mut encoder = DeflateEncoder::new(Vec::new(), compression);
        encoder.write_all(&file_data)?;
        let compressed_data = encoder.finish()?;
        let compressed_size = compressed_data.len() as u64;
        
        // Stocker les données compressées
        all_compressed_data.push(compressed_data);
        
        // Enregistrer l'entrée
        file_entries.push(FileEntry {
            path: file_name.clone(),
            original_size,
            compressed_size,
            crc32,
        });
        
        total_compressed += compressed_size;
        pb.inc(original_size);
    }
    
    pb.finish_with_message("Compression terminée");
    
    // Créer le header final
    let header = MalHeader {
        magic: *MAGIC,
        version: VERSION,
        compression_level: level,
        file_count: files.len() as u32,
        total_original_size: total_size,
        total_compressed_size: total_compressed,
    };
    
    // Écrire le fichier archive
    let mut archive_file = BufWriter::new(File::create(&output)?);
    
    // Écrire le header
    let header_bytes = bincode::serialize(&header)?;
    archive_file.write_all(&(header_bytes.len() as u32).to_le_bytes())?;
    archive_file.write_all(&header_bytes)?;
    
    // Écrire l'index des fichiers
    let index_bytes = bincode::serialize(&file_entries)?;
    archive_file.write_all(&(index_bytes.len() as u32).to_le_bytes())?;
    archive_file.write_all(&index_bytes)?;
    
    // Écrire toutes les données compressées
    for compressed_data in &all_compressed_data {
        archive_file.write_all(&(compressed_data.len() as u32).to_le_bytes())?;
        archive_file.write_all(compressed_data)?;
    }
    
    archive_file.flush()?;
    
    // Afficher les statistiques
    println!("\n{}", "✅ Compression réussie !".bright_green().bold());
    println!("   📄 Fichiers: {}", files.len());
    println!("   📊 Taille originale: {}", format_size(total_size));
    println!("   📦 Taille compressée: {}", format_size(total_compressed));
    println!("   💾 Économie: {:.1}%", 
        100.0 * (1.0 - total_compressed as f64 / total_size as f64));
    println!("   📁 Archive: {}", output.display());
    
    Ok(())
}

// ============================================================================
// DÉCOMPRESSION
// ============================================================================

fn decompress_archive(input: PathBuf, output: PathBuf) -> Result<()> {
    println!("{}", "📂 Lecture de l'archive...".bright_cyan().bold());
    
    let mut file = BufReader::new(File::open(&input)?);
    
    // Lire le header
    let mut header_len_bytes = [0u8; 4];
    file.read_exact(&mut header_len_bytes)?;
    let header_len = u32::from_le_bytes(header_len_bytes) as usize;
    
    let mut header_bytes = vec![0u8; header_len];
    file.read_exact(&mut header_bytes)?;
    let header: MalHeader = bincode::deserialize(&header_bytes)?;
    
    // Vérifier le magic number
    if &header.magic != MAGIC {
        anyhow::bail!("Fichier invalide (magic number incorrect)");
    }
    
    println!("   Version: {}", header.version);
    println!("   Fichiers: {}", header.file_count);
    println!("   Taille originale: {}\n", format_size(header.total_original_size));
    
    // Lire l'index des fichiers
    let mut index_len_bytes = [0u8; 4];
    file.read_exact(&mut index_len_bytes)?;
    let index_len = u32::from_le_bytes(index_len_bytes) as usize;
    
    let mut index_bytes = vec![0u8; index_len];
    file.read_exact(&mut index_bytes)?;
    let file_entries: Vec<FileEntry> = bincode::deserialize(&index_bytes)?;
    
    // Lire tous les fichiers compressés
    let mut compressed_files = Vec::new();
    for _ in 0..header.file_count {
        let mut size_bytes = [0u8; 4];
        file.read_exact(&mut size_bytes)?;
        let size = u32::from_le_bytes(size_bytes) as usize;
        
        let mut data = vec![0u8; size];
        file.read_exact(&mut data)?;
        compressed_files.push(data);
    }
    
    // Créer le dossier de destination
    fs::create_dir_all(&output)?;
    
    // Barre de progression
    let pb = ProgressBar::new(header.total_original_size);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({eta})")
            .unwrap()
            .progress_chars("#>-")
    );
    
    println!("{}", "📤 Décompression en cours...".bright_green().bold());
    
    // Décompresser chaque fichier
    for (i, entry) in file_entries.iter().enumerate() {
        let out_path = output.join(&entry.path);
        
        // Créer les dossiers parents si nécessaire
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }
        
        // Décompresser
        let mut decoder = DeflateDecoder::new(Vec::new());
        decoder.write_all(&compressed_files[i])?;
        let decompressed_data = decoder.finish()?;
        
        // Vérifier le CRC32
        let mut hasher = Hasher::new();
        hasher.update(&decompressed_data);
        let crc32 = hasher.finalize();
        
        if crc32 != entry.crc32 {
            anyhow::bail!("Erreur d'intégrité pour {}: CRC32 invalide", entry.path);
        }
        
        // Écrire le fichier
        let mut out_file = File::create(&out_path)?;
        out_file.write_all(&decompressed_data)?;
        
        pb.inc(entry.original_size);
    }
    
    pb.finish_with_message("Décompression terminée");
    
    println!("\n{}", "✅ Décompression réussie !".bright_green().bold());
    println!("   📁 Destination: {}", output.display());
    
    Ok(())
}

// ============================================================================
// LISTING
// ============================================================================

fn list_archive(input: PathBuf) -> Result<()> {
    let mut file = BufReader::new(File::open(&input)?);
    
    // Lire le header
    let mut header_len_bytes = [0u8; 4];
    file.read_exact(&mut header_len_bytes)?;
    let header_len = u32::from_le_bytes(header_len_bytes) as usize;
    
    let mut header_bytes = vec![0u8; header_len];
    file.read_exact(&mut header_bytes)?;
    let header: MalHeader = bincode::deserialize(&header_bytes)?;
    
    if &header.magic != MAGIC {
        anyhow::bail!("Fichier invalide");
    }
    
    println!("\n{}", "📦 Archive MAL".bright_cyan().bold());
    println!("{}", "═".repeat(80).bright_black());
    println!("  Version: {}", header.version);
    println!("  Niveau de compression: {}", header.compression_level);
    println!("  Nombre de fichiers: {}", header.file_count);
    println!("  Taille originale: {}", format_size(header.total_original_size));
    println!("  Taille compressée: {}", format_size(header.total_compressed_size));
    println!("  Ratio: {:.1}%", 
        100.0 * header.total_compressed_size as f64 / header.total_original_size as f64);
    
    // Lire l'index
    let mut index_len_bytes = [0u8; 4];
    file.read_exact(&mut index_len_bytes)?;
    let index_len = u32::from_le_bytes(index_len_bytes) as usize;
    
    let mut index_bytes = vec![0u8; index_len];
    file.read_exact(&mut index_bytes)?;
    let file_entries: Vec<FileEntry> = bincode::deserialize(&index_bytes)?;
    
    println!("\n{}", "📄 Fichiers".bright_yellow().bold());
    println!("{}", "─".repeat(80).bright_black());
    println!("{:<50} {:>12} {:>12} {:>8}", "Chemin", "Original", "Compressé", "Ratio");
    println!("{}", "─".repeat(80).bright_black());
    
    for entry in file_entries {
        let ratio = 100.0 * entry.compressed_size as f64 / entry.original_size as f64;
        println!("{:<50} {:>12} {:>12} {:>7.1}%",
            truncate_path(&entry.path, 50),
            format_size(entry.original_size),
            format_size(entry.compressed_size),
            ratio
        );
    }
    
    println!("{}\n", "═".repeat(80).bright_black());
    
    Ok(())
}

// ============================================================================
// VÉRIFICATION
// ============================================================================

fn verify_archive(input: PathBuf) -> Result<()> {
    println!("{}", "🔍 Vérification de l'intégrité...".bright_cyan().bold());
    
    let mut file = BufReader::new(File::open(&input)?);
    
    // Lire le header
    let mut header_len_bytes = [0u8; 4];
    file.read_exact(&mut header_len_bytes)?;
    let header_len = u32::from_le_bytes(header_len_bytes) as usize;
    
    let mut header_bytes = vec![0u8; header_len];
    file.read_exact(&mut header_bytes)?;
    let header: MalHeader = bincode::deserialize(&header_bytes)?;
    
    if &header.magic != MAGIC {
        anyhow::bail!("❌ Fichier invalide (magic number incorrect)");
    }
    
    println!("   ✓ Magic number valide");
    println!("   ✓ Version: {}", header.version);
    
    // Lire l'index
    let mut index_len_bytes = [0u8; 4];
    file.read_exact(&mut index_len_bytes)?;
    let index_len = u32::from_le_bytes(index_len_bytes) as usize;
    
    let mut index_bytes = vec![0u8; index_len];
    file.read_exact(&mut index_bytes)?;
    let file_entries: Vec<FileEntry> = bincode::deserialize(&index_bytes)?;
    
    println!("   ✓ Index lu");
    
    // Lire les fichiers compressés
    let mut compressed_files = Vec::new();
    for i in 0..header.file_count {
        let mut size_bytes = [0u8; 4];
        file.read_exact(&mut size_bytes)
            .context(format!("Erreur lecture taille fichier {}", i))?;
        let size = u32::from_le_bytes(size_bytes) as usize;
        
        let mut data = vec![0u8; size];
        file.read_exact(&mut data)
            .context(format!("Erreur lecture données fichier {}", i))?;
        compressed_files.push(data);
    }
    
    println!("   ✓ {} fichiers lus\n", header.file_count);
    
    // Barre de progression
    let pb = ProgressBar::new(header.file_count as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} fichiers")
            .unwrap()
            .progress_chars("#>-")
    );
    
    let mut errors = Vec::new();
    
    // Vérifier chaque fichier
    for (i, entry) in file_entries.iter().enumerate() {
        // Décompresser
        let mut decoder = DeflateDecoder::new(Vec::new());
        match decoder.write_all(&compressed_files[i]) {
            Ok(_) => {},
            Err(e) => {
                errors.push(format!("{}: erreur décompression ({})", entry.path, e));
                pb.inc(1);
                continue;
            }
        }
        
        let decompressed_data = match decoder.finish() {
            Ok(d) => d,
            Err(e) => {
                errors.push(format!("{}: erreur décompression ({})", entry.path, e));
                pb.inc(1);
                continue;
            }
        };
        
        // Vérifier la taille
        if decompressed_data.len() as u64 != entry.original_size {
            errors.push(format!("{}: taille incorrecte", entry.path));
        }
        
        // Vérifier le CRC32
        let mut hasher = Hasher::new();
        hasher.update(&decompressed_data);
        let crc32 = hasher.finalize();
        
        if crc32 != entry.crc32 {
            errors.push(format!("{}: CRC32 invalide", entry.path));
        }
        
        pb.inc(1);
    }
    
    pb.finish_and_clear();
    
    if errors.is_empty() {
        println!("\n{}", "✅ Archive valide ! Tous les fichiers sont intacts.".bright_green().bold());
    } else {
        println!("\n{}", "❌ Erreurs détectées :".bright_red().bold());
        for error in errors {
            println!("   • {}", error.red());
        }
        anyhow::bail!("Vérification échouée");
    }
    
    Ok(())
}

// ============================================================================
// UTILITAIRES
// ============================================================================

fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit_idx = 0;
    
    while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
        size /= 1024.0;
        unit_idx += 1;
    }
    
    if unit_idx == 0 {
        format!("{} {}", bytes, UNITS[0])
    } else {
        format!("{:.2} {}", size, UNITS[unit_idx])
    }
}

fn truncate_path(path: &str, max_len: usize) -> String {
    if path.len() <= max_len {
        path.to_string()
    } else {
        format!("...{}", &path[path.len() - (max_len - 3)..])
    }
}

// ============================================================================
// BINCODE (simple serialization)
// ============================================================================

mod bincode {
    use serde::{Serialize, Deserialize};
    use anyhow::Result;
    
    pub fn serialize<T: Serialize>(value: &T) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(value)?)
    }
    
    pub fn deserialize<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T> {
        Ok(serde_json::from_slice(bytes)?)
    }
}

// ============================================================================
// MAIN
// ============================================================================

fn main() -> Result<()> {
    let cli = Cli::parse();
    
    match cli.command {
        Commands::Compress { inputs, output, level } => {
            compress_files(inputs, output, level)?;
        }
        Commands::Decompress { input, output } => {
            decompress_archive(input, output)?;
        }
        Commands::List { input } => {
            list_archive(input)?;
        }
        Commands::Verify { input } => {
            verify_archive(input)?;
        }
    }
    
    Ok(())
}
