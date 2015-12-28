extern crate lz4;
extern crate filetime;

use std::cmp::min;
use std::env;
use std::fmt::{Display, Formatter};
use std::ffi::OsString;
use std::fs;
use std::fs::{File, OpenOptions};
use std::io::{Error, ErrorKind, Read, Write, Seek, SeekFrom};
use std::hash::{Hasher, SipHasher};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use self::filetime::FileTime;

use super::super::cache::FileHasher;
use super::super::compiler::OutputInfo;
use super::super::utils::DEFAULT_BUF_SIZE;
use super::binary::*;
use super::statistic::Statistic;
use super::counter::Counter;

const HEADER: &'static [u8] = b"OBCF\x00\x03";
const FOOTER: &'static [u8] = b"END\x00";
const SUFFIX: &'static str = ".lz4";

#[derive(Debug)]
pub enum CacheError {
	InvalidHeader(PathBuf),
	InvalidFooter(PathBuf),
	PackedFilesMismatch(PathBuf),
	MutexError(String),
}

impl Display for CacheError {
	fn fmt(&self, f: &mut Formatter) -> Result<(), ::std::fmt::Error> {
		match self {
			&CacheError::InvalidHeader(ref path) => write!(f, "invalid cache file header: {}", path.display()),
			&CacheError::InvalidFooter(ref path) => write!(f, "invalid cache file footer: {}", path.display()),
			&CacheError::PackedFilesMismatch(ref path) => write!(f, "unexpected count of packed cached files: {}", path.display()),
			&CacheError::MutexError(ref message) => write!(f, "mutex error: {}", message),
		}
	}
}

impl ::std::error::Error for CacheError {
	fn description(&self) -> &str {
		match self {
			&CacheError::InvalidHeader(_) => "invalid cache file header",
			&CacheError::InvalidFooter(_) => "invalid cache file footer",
			&CacheError::PackedFilesMismatch(_) => "unexpected count of packed cached files",
			&CacheError::MutexError(_) => "mutex error",
		}
	}

	fn cause(&self) -> Option<&::std::error::Error> {
		None
	}
}

#[derive(Clone)]
pub struct FileCache {
	cache_dir: PathBuf,
}

struct CacheFile {
	path: PathBuf,
	size: u64,
	accessed: FileTime,
}

impl FileCache {
	pub fn new() -> Self {
		let cache_dir = match env::var("OCTOBUILD_CACHE") {
			Ok(value) => Path::new(&value).to_path_buf(),
			Err(_) => env::home_dir().unwrap().join(".octobuild").join("cache")
		};
		FileCache {
			cache_dir: cache_dir,
		}
	}

	pub fn run_cached<F: Fn()->Result<OutputInfo, Error>, C: Fn()->bool>(&self, file_hasher: &FileHasher, statistic: &RwLock<Statistic>, params: u64, inputs: &Vec<PathBuf>, outputs: &Vec<PathBuf>, worker: F, checker: C) -> Result<OutputInfo, Error> {
		let hash = try! (self.generate_hash(file_hasher, params, inputs));
		let path = self.cache_dir.join(&hash[0..2]).join(&(hash[2..].to_string() + SUFFIX));
		// Try to read data from cache.
		match read_cache(statistic, &path, outputs) {
			Ok(output) => {return Ok(output)}
			Err(_) => {}
		}
		// Run task and save result to cache.
		let output = try !(worker());
		if checker() {
			try !(write_cache(statistic, &path, outputs, &output));
		}
		Ok(output)
	}

	pub fn cleanup(&self, max_cache_size: u64) -> Result<(), Error> {
		let mut files = try! (find_cache_files(&self.cache_dir, Vec::new()));
		files.sort_by(|a, b| b.accessed.cmp(&a.accessed));
		
		let mut cache_size: u64 = 0;
		for item in files.into_iter() {
			cache_size += item.size;
			if cache_size > max_cache_size {
				let _ = try!(fs::remove_file(&item.path));
			}
		}
		Ok(())
	}

	fn generate_hash(&self, file_hasher: &FileHasher, params: u64, inputs: &Vec<PathBuf>) -> Result<String, Error> {
		let mut hash = SipHasher::new();
		// params
		hash.write_u64(params);
		// inputs
		for input in inputs.iter() {
			let file_hash = try! (file_hasher.file_hash(input));
			hash.write(file_hash.as_bytes());
		}
		Ok(format!("{:016x}", hash.finish()))
	}
}

fn find_cache_files(dir: &Path, mut files: Vec<CacheFile>) -> Result<Vec<CacheFile>, Error> {
	for entry in try!(fs::read_dir(dir)) {
		let entry = try!(entry);
		let path = entry.path();
		let stat = try! (fs::metadata(&path));
		if stat.is_dir() {
			let r = find_cache_files(&path, files);
			files = try! (r);
		} else {
			files.push(CacheFile {
				path: path,
				size: stat.len(),
				accessed: FileTime::from_last_modification_time(&stat),
			});
		}
	}
	Ok(files)
}

fn write_cached_file<W: Write>(stream: &mut W, path: &PathBuf) -> Result<(), Error> {
	let mut buf: [u8; DEFAULT_BUF_SIZE] = [0; DEFAULT_BUF_SIZE];
	let mut file = try! (File::open(path));
	let total_size = try! (file.seek(SeekFrom::End(0)));
	try! (file.seek(SeekFrom::Start(0)));
	try! (write_u64(stream, total_size));
	let mut need_size = total_size;
	loop {
		let size = try! (file.read(&mut buf));
		if size == 0 && need_size == 0 {
			break;
		}
		if size <= 0 {
			return Err(Error::new(ErrorKind::BrokenPipe, "Unexpected end of stream"));
		}
		if need_size < size as u64 {
			return Err(Error::new(ErrorKind::BrokenPipe, "Expected end of stream"));
		}
		try! (stream.write_all(&buf[0..size]));
		need_size = need_size - (size as u64);
	}
	Ok(())
}

fn write_cache(statistic: &RwLock<Statistic>, path: &Path, paths: &Vec<PathBuf>, output: &OutputInfo) -> Result<(), Error> {
	if !output.success() {
		return Ok(());
	}
	match path.parent() {
		Some(parent) => try! (fs::create_dir_all(&parent)),
		None => ()
	}
	let mut stream = try! (lz4::EncoderBuilder::new().level(1).build(Counter::writer(try! (File::create(path)))));
	try! (stream.write_all(HEADER));
	try! (write_usize(&mut stream, paths.len()));
	for path in paths.iter() {
		try! (write_cached_file(&mut stream, path));
	}
	try! (write_output(&mut stream, output));
	try! (stream.write_all(FOOTER));
	match stream.finish() {
		(writer, result) => {
			statistic.write().unwrap().add_miss(writer.len());
			result
		}
	}
}

fn read_cached_file<R: Read>(stream: &mut R, path: &PathBuf) -> Result<(), Error> {
	let mut buf: [u8; DEFAULT_BUF_SIZE] = [0; DEFAULT_BUF_SIZE];
	let total_size = try! (read_u64(stream));
	let mut need_size = total_size;

	let mut file = try! (File::create(path));
	try! (file.set_len(total_size as u64));
	while  need_size > 0 {
		let need = min(buf.len() as u64, need_size) as usize;
		let size = try! (stream.read(&mut buf[0..need]));
		if size == 0 {
			return Err(Error::new(ErrorKind::BrokenPipe, "Expected end of stream"));
		}
		try! (file.write_all(&buf[0..size]));
		need_size = need_size - (size as u64);
	}
	Ok(())
}

fn read_cache(statistic: &RwLock<Statistic>, path: &Path, paths: &Vec<PathBuf>) -> Result<OutputInfo, Error> {
	let mut file = try! (OpenOptions::new().read(true).write(true).open(Path::new(path)));
	try! (file.write(&[4]));
	try! (file.seek(SeekFrom::Start(0)));
	let mut stream = try! (lz4::Decoder::new (Counter::reader(file)));
	if try! (read_exact(&mut stream, HEADER.len())) != HEADER {
		return Err(Error::new(ErrorKind::InvalidInput, CacheError::InvalidHeader(path.to_path_buf())));
	}
	if try! (read_usize(&mut stream)) != paths.len() {
		return Err(Error::new(ErrorKind::InvalidInput, CacheError::PackedFilesMismatch(path.to_path_buf())));
	} 
	for path in paths.iter() {
		let mut temp_name = OsString::from("~tmp~");
		temp_name.push(path.file_name().unwrap());
		let temp = path.with_file_name(temp_name);
		drop(fs::remove_file(&path));
		match read_cached_file(&mut stream, &temp).and_then(|_| fs::rename(&temp, &path)) {
			Ok(_) => {
			}
			Err(e) => {
				drop(fs::remove_file(&temp));
				return Err(e);
			}
		};
	}
	let output = try! (read_output(&mut stream));
	if try! (read_exact(&mut stream, FOOTER.len())) != FOOTER {
		return Err(Error::new(ErrorKind::InvalidInput, CacheError::InvalidFooter(path.to_path_buf())));
	}
	let mut eof = [0];
	if try! (stream.read(&mut eof)) != 0 {
		return Err(Error::new(ErrorKind::InvalidInput, CacheError::InvalidFooter(path.to_path_buf())));
	}
	statistic.write().unwrap().add_hit(stream.finish().0.len());
	Ok(output)
}

fn write_blob(stream: &mut Write, blob: &[u8]) -> Result<(), Error> {
	try! (write_usize(stream, blob.len()));
	try! (stream.write_all(blob));
	Ok(())
}

fn read_blob(stream: &mut Read) -> Result<Vec<u8>, Error> {
	let size = try! (read_usize(stream));
	read_exact(stream, size)
}

fn write_output(stream: &mut Write, output: &OutputInfo) -> Result<(), Error> {
	try! (write_blob(stream, &output.stdout));
	try! (write_blob(stream, &output.stderr));
	Ok(())
}

fn read_output(stream: &mut Read) -> Result<OutputInfo, Error> {
	let stdout = try! (read_blob(stream));
	let stderr = try! (read_blob(stream));
	Ok(OutputInfo {
		status: Some(0),
		stdout: stdout,
		stderr: stderr,
	})
}
