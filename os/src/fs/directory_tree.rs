use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use super::fat32::{DiskInodeType, EasyFileSystem};
use lazy_static::*;
use spin::{Mutex, RwLock, RwLockWriteGuard, MutexGuard};

use super::{
    dev::{null::Null, tty::Teletype, zero::Zero},
    file_trait::File,
    filesystem::FileSystem,
    layout::OpenFlags, Hwclock,
    cache::{BlockCacheManager}
};
use crate::{syscall::errno::*, mm::tlb_invalidate};
use crate::{
    drivers::BLOCK_DEVICE,
    fs::{
        filesystem::FS,
        fat32::{
            inode::{InodeImpl, OSInode},
        },
    },
};

lazy_static! {
    pub static ref FILE_SYSTEM: Arc<EasyFileSystem> = EasyFileSystem::open(
        BLOCK_DEVICE.clone(),
        Arc::new(Mutex::new(BlockCacheManager::new()))
    );//指向文件系统的索引
    pub static ref ROOT: Arc<DirectoryTreeNode> = {
        let inode = DirectoryTreeNode::new(
            "".to_string(),
            Arc::new(FileSystem::new(FS::Fat32)),
            OSInode::new(InodeImpl::root_inode(&FILE_SYSTEM)),
            Weak::new()
        );//create一个目录树节点
        inode.add_special_use();//引用次数加一
        inode
    };//根目录的索引
    static ref DIRECTORY_VEC: Mutex<(Vec<Weak<DirectoryTreeNode>>, usize)> = Mutex::new((Vec::new(), 0));//创建一个目录向量，进行存储
    static ref PATH_CACHE: Mutex<(String, Weak<DirectoryTreeNode>)> = Mutex::new(("".to_string(), Weak::new()));//路径
}

fn insert_directory_vec(inode: Weak<DirectoryTreeNode>) {
    DIRECTORY_VEC.lock().0.push(inode);
}
fn delete_directory_vec() {
    let mut lock = DIRECTORY_VEC.lock();
    lock.1 += 1;
    if lock.1 >= lock.0.len() / 2 {
        update_directory_vec(&mut lock);
    }
}
fn update_directory_vec(lock: &mut MutexGuard<(Vec<Weak<DirectoryTreeNode>>, usize)>) {
    let mut new_vec: Vec<Weak<DirectoryTreeNode>> = Vec::new();
    for inode in &lock.0 {
        if inode.upgrade().is_some() {
            new_vec.push(inode.clone());
        }
    }
    **lock = (new_vec, 0);
}

pub struct DirectoryTreeNode {
    /// If this is a directory
    /// 1. cwd
    /// 2. mount point
    /// 3. root node
    /// If this is a file
    /// 1. executed by some processes
    /// This parameter will add 1 when opening
    spe_usage: Mutex<usize>,
    name: String,
    filesystem: Arc<FileSystem>,
    file: Arc<dyn File>,
    selfptr: Mutex<Weak<Self>>,
    father: Mutex<Weak<Self>>,
    children: RwLock<Option<BTreeMap<String, Arc<Self>>>>,
}

impl Drop for DirectoryTreeNode {
    fn drop(&mut self) {
        delete_directory_vec();
    }
}

impl DirectoryTreeNode {
    pub fn new(
        name: String,
        filesystem: Arc<FileSystem>,
        file: Arc<dyn File>,
        father: Weak<Self>,
    ) -> Arc<Self> {
        let node = Arc::new(DirectoryTreeNode {
            spe_usage: Mutex::new(0),
            name,
            filesystem,
            file,
            selfptr: Mutex::new(Weak::new()),
            father: Mutex::new(father),
            children: RwLock::new(None),
        });
        *node.selfptr.lock() = Arc::downgrade(&node);
        node.file.info_dirtree_node(Arc::downgrade(&node));
        insert_directory_vec(Arc::downgrade(&node));
        node
    }
    pub fn add_special_use(&self) {
        *self.spe_usage.lock() += 1;
    }
    pub fn sub_special_use(&self) {
        *self.spe_usage.lock() -= 1;
    }
    pub fn get_cwd(&self) -> String {
        let mut pathv = Vec::<String>::with_capacity(8);//构建一个路径向量
        let mut current_inode = self.get_arc();//获取当前目录的节点
        loop {
            let lock = current_inode.father.lock();//将当前目录的父目录上锁
            let par_inode = match lock.upgrade() {
                Some(inode) => inode.clone(),
                None => break,
            };
            drop(lock);
            pathv.push(current_inode.name.clone());//将当前目录的路径放入pathv中
            current_inode = par_inode;//获取当前目录节点
        }
        pathv.push(current_inode.name.clone());
        pathv.reverse();
        if pathv.len() == 1 {
            "/".to_string()//根目录
        } else {
            pathv.join("/")
        }
    }
    fn get_arc(&self) -> Arc<Self> {
        self.selfptr.lock().upgrade().unwrap().clone()
    }
    fn parse_dir_path(path: &str) -> Vec<&str> {
        path.split('/').fold(Vec::with_capacity(8), |mut v, s| {
            match s {
                "" | "." => {}
                ".." => {
                    if v.last().map_or(true, |s| *s == "..") {
                        v.push(s);
                    } else {
                        v.pop();
                    }
                }
                _ => {
                    v.push(s);
                }
            }
            v
        })
    }
    fn cache_all_subfile(
        &self,
        lock: &mut RwLockWriteGuard<Option<BTreeMap<String, Arc<Self>>>>,
    ) -> Result<(), isize> {
        if lock.is_some() {
            return Ok(())
        }
        if !self.file.is_dir() {
            return Err(ENOTDIR);
        }//当前文件不是目录直接返回错误
        let vec = match self.file.open_subfile() {
            Ok(vec) => vec,
            Err(errno) => return Err(errno),
        };//打开子文件，失败则返回错误
        let mut map = BTreeMap::new();//建立一个新的映射
        for (name, file) in vec {
            let key = name.clone();
            let value = Self::new(
                key.clone(),
                self.filesystem.clone(),
                file.clone(),
                Arc::downgrade(&self.get_arc())
            );
            map.insert(key, value);//将name和新节点放入新创建的map中
        }
        **lock = Some(map);
        Ok(())
    }
    fn try_to_open_subfile(
        &self,
        name: &str,
        lock: &mut RwLockWriteGuard<Option<BTreeMap<String, Arc<Self>>>>,
    ) -> Result<Arc<Self>, isize> {
        match self.cache_all_subfile(lock) {
            Ok(_) => {},
            Err(errno) => return Err(errno),
        };
        match lock.as_ref().unwrap().get(&name.to_string()) {
            Some(child) => Ok(child.clone()),
            None => Err(ENOENT),
        }
    }
    pub fn cd_comp(&self, components: &Vec<&str>) -> Result<Arc<Self>, isize> {
        let mut current_inode = self.get_arc();
        for component in components {
            if *component == ".." {//根目录
                let lock = current_inode.father.lock();
                let par_inode = lock.upgrade();
                match par_inode {
                    Some(par_inode) => {
                        drop(lock);
                        current_inode = par_inode;//将父节点赋值给当前节点，即来到了父目录
                    },
                    None => {},
                }
                continue;
            }
            //不是根目录，那么就进入到相应的子文件中
            let mut lock = current_inode.children.write();
            match current_inode.try_to_open_subfile(component, &mut lock) {
                Ok(child_inode) => {
                    let child_inode = child_inode.clone();
                    drop(lock);
                    current_inode = child_inode.clone()
                },
                Err(errno) => return Err(errno),
            }
        }
        Ok(current_inode)
    }
    pub fn cd_path(&self, path: &str) -> Result<Arc<Self>, isize> {
        let components = Self::parse_dir_path(path);
        let inode = if path.starts_with("/") {
            &**ROOT
        } else {
            &self
        };
        inode.cd_comp(&components)
    }
    fn create(&self, name: &str, file_type: DiskInodeType) -> Result<Arc<dyn File>, isize> {
        // if name == "" || !self.file.is_dir() {
        //     debug_assert!(false);
        // }
        self.file.create(name, file_type)
    }
    pub fn open(
        &self,
        path: &str,//打开文件的路径
        flags: OpenFlags,//文件的打开标志，表明文件是只读只写等属性，定义在layout.rs文件中，在这里不再叙述
        special_use: bool,//指名文件是否已经打开
    ) -> Result<Arc<dyn File>, isize> {
        log::debug!("[open]: cwd: {}, path: {}", self.get_cwd(), path);
        //确定路径
        const BUSYBOX_PATH: &str = "/busybox";
        const REDIRECT_TO_BUSYBOX: [&str; 3] = ["/touch", "/rm", "/ls"];
        let path = if REDIRECT_TO_BUSYBOX.contains(&path) {
            BUSYBOX_PATH
        } else {
            path
        };
        const LIBC_PATH: &str = "/lib/libc.so";
        const REDIRECT_TO_LIBC: [&str; 3] = [
            "/lib/ld-musl-riscv64.so.1",
            "/lib/ld-musl-riscv64-sf.so.1",
            "/lib/ld-linux-riscv64-lp64d.so.1",
        ];
        let path = if REDIRECT_TO_LIBC.contains(&path) {
            LIBC_PATH
        } else {
            path
        };
        let path = if path == "/usr/lib/tls_get_new-dtv_dso.so" {
            "./libtls_get_new-dtv_dso.so"
        } else {
            path
        };
        let inode = if path.starts_with("/") {
            &**ROOT
        } else {
            &self
        };
        
        let mut path_cache_lock = PATH_CACHE.lock();
        let inode = if path.starts_with('/') && path == path_cache_lock.0 && path_cache_lock.1.upgrade().is_some() {
            path_cache_lock.1.upgrade().unwrap()
        }
        else {
            let mut components = Self::parse_dir_path(path);
            let last_comp = components.pop();
            let inode = match inode.cd_comp(&components) {
                Ok(inode) => inode,
                Err(errno) => return Err(errno),
            };
            if let Some(last_comp) = last_comp {
                let mut lock = inode.children.write();
                match inode.try_to_open_subfile(last_comp, &mut lock) {
                    //要打开的文件存在
                    Ok(inode) => {
                        if flags.contains(OpenFlags::O_CREAT | OpenFlags::O_EXCL) {
                            return Err(EEXIST);
                        }
                        inode
                    }
                    //文件不存在就返回错误，新建一个索引节点
                    Err(ENOENT) => {
                        if !flags.contains(OpenFlags::O_CREAT) {
                            return Err(ENOENT);
                        }
                        let new_file = match inode.create(last_comp, DiskInodeType::File) {
                            Ok(file) => file,
                            Err(errno) => return Err(errno),
                        };
                        let key = (*last_comp).to_string();
                        let value = Self::new(
                            key.clone(),
                            inode.filesystem.clone(),
                            new_file,
                            Arc::downgrade(&inode.get_arc()),
                        );
                        let new_inode = value.clone();
                        lock.as_mut().unwrap().insert(key, value);
                        new_inode
                    }
                    Err(errno) => {
                        return Err(errno);
                    }
                }
            } else {
                inode
            }
        };
	//要打开的文件以只写、读写方式打开，则将其长度截断为0。
        if flags.contains(OpenFlags::O_TRUNC) {
            match inode.file.truncate_size(0) {
                Ok(_) => {}
                Err(errno) => return Err(errno),
            }
        }
	//文件正在被使用，文本文件繁忙
        if inode.file.is_file()
            && *inode.spe_usage.lock() > 0
            && (flags.contains(OpenFlags::O_WRONLY) || flags.contains(OpenFlags::O_RDWR))
        {
            return Err(ETXTBSY);
        }
	//对文件夹进行文件操作
        if inode.file.is_dir()
            && (flags.contains(OpenFlags::O_WRONLY) || flags.contains(OpenFlags::O_RDWR))
        {
            return Err(EISDIR);
        }
	//不是目录
        if !inode.file.is_dir() && flags.contains(OpenFlags::O_DIRECTORY) {
            return Err(ENOTDIR);
        }
	//文件被打开，参数spe_usage+1
        if special_use {
            *inode.spe_usage.lock() += 1;
        }
	
        if path.starts_with('/') && path != path_cache_lock.0 {
            *path_cache_lock = (path.to_string(), Arc::downgrade(&inode.get_arc()));
        }

        Ok(inode.file.open(flags, special_use))
    }

    pub fn mkdir(&self, path: &str) -> Result<(), isize> {
        let inode = if path.starts_with("/") {
            &**ROOT
        } else {
            &self
        };

        let mut components = Self::parse_dir_path(path);
        let last_comp = components.pop();
        let inode = match inode.cd_comp(&components) {
            Ok(inode) => inode,
            Err(errno) => return Err(errno),
        };

        if let Some(last_comp) = last_comp {
            let mut lock = inode.children.write();
            match inode.try_to_open_subfile(last_comp, &mut lock) {
                Ok(_) => {
                    return Err(EEXIST);
                }
                Err(ENOENT) => {
                    let new_file = match inode.create(last_comp, DiskInodeType::Directory) {
                        Ok(file) => file,
                        Err(errno) => return Err(errno),
                    };
                    let key = (*last_comp).to_string();
                    let value = Self::new(
                        key.clone(),
                        inode.filesystem.clone(),
                        new_file,
                        Arc::downgrade(&inode.get_arc()),
                    );
                    let new_inode = value.clone();
                    lock.as_mut().unwrap().insert(key, value);
                    new_inode
                }
                Err(errno) => {
                    return Err(errno);
                }
            }
        } else {
            return Err(EEXIST);
        };

        Ok(())
    }

    pub fn delete(&self, path: &str, delete_directory: bool) -> Result<(), isize> {
        if path.split('/').last().map_or(true, |x| x == ".") {
            return Err(EINVAL);
        }
	//根目录
        let inode = if path.starts_with("/") {
            &**ROOT
        } else {
            &self
        };

        let components = Self::parse_dir_path(path);
        let last_comp = *components.last().unwrap();
        let inode = match inode.cd_comp(&components) {
            Ok(inode) => inode,
            Err(errno) => return Err(errno),
        };
	//文件正在打开，不能删除
        if *inode.spe_usage.lock() > 0 {
            return Err(EBUSY);
        }
	//要删除的目录里面是一个文件夹，不能直接删除，需要逐层删除
        if !delete_directory && inode.file.is_dir() {
            return Err(EISDIR);
        }
	//要删除的目录是一个文件夹，不能直接删除
        if delete_directory && !inode.file.is_dir() {
            return Err(ENOTDIR);
        }
	//删除文件
        match inode.father.lock().upgrade() {
            Some(par_inode) => {
                let mut lock = par_inode.children.write();
                match inode.file.unlink(true) {
                    Ok(_) => {
                        let key = last_comp.to_string();
                        lock.as_mut().unwrap().remove(&key);
                    }
                    Err(errno) => return Err(errno),
                }
            }
            None => return Err(EACCES),
        }
        Ok(())
    }

    pub fn rename(old_path: &str, new_path: &str) -> Result<(), isize> {
        assert!(old_path.starts_with('/'));
        assert!(new_path.starts_with('/'));

        let mut old_comps = Self::parse_dir_path(old_path);
        let mut new_comps = Self::parse_dir_path(new_path);
	//新旧文件名一样
        if old_comps == new_comps {
            return Ok(());
        }
	//不能是旧文件名
        if new_comps.starts_with(&old_comps) {
            return Err(EINVAL);
        }
        // We gurantee that last component isn't empty
        let old_last_comp = old_comps.pop().unwrap();
        let new_last_comp = new_comps.pop().unwrap();

        let old_par_inode = match ROOT.cd_comp(&old_comps) {
            Ok(inode) => inode,
            Err(errno) => return Err(errno),
        };
        let new_par_inode = match ROOT.cd_comp(&new_comps) {
            Ok(inode) => inode,
            Err(errno) => return Err(errno),
        };
        type ChildLockType<'a> = RwLockWriteGuard<'a, Option<BTreeMap<String, Arc<DirectoryTreeNode>>>>;

        let old_lock: Arc<Mutex<ChildLockType<'_>>>;
        let new_lock: Arc<Mutex<ChildLockType<'_>>>;

        // Be careful about the lock ordering
        if old_comps == new_comps {
            old_lock = Arc::new(Mutex::new(old_par_inode.children.write()));
            new_lock = old_lock.clone();
        } else if old_comps < new_comps {
            old_lock = Arc::new(Mutex::new(old_par_inode.children.write()));
            new_lock = Arc::new(Mutex::new(new_par_inode.children.write()));
        } else {
            new_lock = Arc::new(Mutex::new(new_par_inode.children.write()));
            old_lock = Arc::new(Mutex::new(old_par_inode.children.write()));
        }

        let old_inode =
            match old_par_inode.try_to_open_subfile(old_last_comp, &mut (*old_lock.lock())) {
                Ok(inode) => inode,
                Err(errno) => return Err(errno),
            };
	//被修改文件正在打开，不能被修改
        if *old_inode.spe_usage.lock() > 0 {
            return Err(EBUSY);
        }
	//oldname与newname必须属于同一个文件系统，否则会返回-EXDEV错误。这是因为，inode number只在一个文件系统内唯一，跨文件系统没有意义，所以oldname与newname必须在同一个文件系统中
        if old_inode.filesystem.fs_id != new_par_inode.filesystem.fs_id {
            return Err(EXDEV);
        }
        let old_key = old_last_comp.to_string();
        let new_key = new_last_comp.to_string();
        match new_par_inode.try_to_open_subfile(new_last_comp, &mut (*new_lock.lock())) {
            Ok(new_inode) => {
            	//文件类型不匹配
                if new_inode.file.is_dir() && !old_inode.file.is_dir() {
                    return Err(EISDIR);
                }
                if old_inode.file.is_dir() && !new_inode.file.is_dir() {
                    return Err(ENOTDIR);
                }
                //文件正在打开中
                if *new_inode.spe_usage.lock() > 0 {
                    return Err(EBUSY);
                }
                // delete，断开新节点和之前数据区的指向关系
                match new_par_inode.file.unlink(true) {
                    Ok(_) => {
                        new_lock.lock().as_mut().unwrap().remove(&new_key);
                    }
                    Err(errno) => return Err(errno),
                }
            }
            Err(ENOENT) => {}
            Err(errno) => return Err(errno),
        }
	//获取要修改文件
        let value = old_lock.lock().as_mut().unwrap().remove(&old_key).unwrap();
        match old_inode.file.unlink(false) {//断开要修改文件和指向数据区的关系
            Ok(_) => {}
            Err(errno) => return Err(errno),
        };
        match old_inode.filesystem.fs_type {
            FS::Fat32 => {
                let old_file = old_inode.file.downcast_ref::<OSInode>().unwrap();
                let new_par_file = new_par_inode.file.downcast_ref::<OSInode>().unwrap();
                new_par_file.link_child(old_last_comp, old_file)?;
            }
            FS::Null => return Err(EACCES),
        }
        //重新建立新节点和之前数据区的联系
        *value.father.lock() = Arc::downgrade(&new_par_inode.get_arc());
        new_lock.lock().as_mut().unwrap().insert(new_key, value);

        Ok(())
    }
}

pub fn oom() -> usize {
    tlb_invalidate();
    const MAX_FAIL_TIME: usize = 3;
    let mut fail_time = 0;
    log::warn!("[oom] start oom");
    let mut lock = DIRECTORY_VEC.lock();
    update_directory_vec(&mut lock);
    loop {
        let mut dropped = 0;
        for inode in &lock.0 {
            let inode = inode.upgrade().unwrap();
            dropped += inode.file.oom();
        }
        if dropped > 0 {
            log::warn!("[oom] recycle pages: {}", dropped);
            return dropped;
        }
        fail_time += 1;
        if fail_time >= MAX_FAIL_TIME {
            return dropped;
        }
    }
}

pub fn init_fs() {
    init_device_directory();
    init_tmp_directory();
    init_proc_directory();
}
#[allow(unused)]
fn init_device_directory() {
    ROOT.mkdir("/dev");

    let dev_inode = match ROOT.cd_path("/dev") {
        Ok(inode) => inode,
        Err(_) => panic!("dev directory doesn't exist"),
    };

    dev_inode.mkdir("shm");
    dev_inode.mkdir("misc");

    let null_dev = DirectoryTreeNode::new(
        "null".to_string(),
        Arc::new(FileSystem::new(FS::Null)),
        Arc::new(Null {}),
        Arc::downgrade(&dev_inode.get_arc()),
    );
    let zero_dev = DirectoryTreeNode::new(
        "zero".to_string(),
        Arc::new(FileSystem::new(FS::Null)),
        Arc::new(Zero {}),
        Arc::downgrade(&dev_inode.get_arc()),
    );
    let tty_dev = DirectoryTreeNode::new(
        "tty".to_string(),
        Arc::new(FileSystem::new(FS::Null)),
        Arc::new(Teletype::new()),
        Arc::downgrade(&dev_inode.get_arc()),
    );
    let mut lock = dev_inode.children.write();
    lock.as_mut().unwrap().insert("null".to_string(), null_dev);
    lock.as_mut().unwrap().insert("zero".to_string(), zero_dev);
    lock.as_mut().unwrap().insert("tty".to_string(), tty_dev);
    drop(lock);

    let misc_inode = match dev_inode.cd_path("./misc") {
        Ok(inode) => inode,
        Err(_) => panic!("misc directory doesn't exist"),
    };
    let hwclock_dev = DirectoryTreeNode::new(
        "rtc".to_string(),
        Arc::new(FileSystem::new(FS::Null)),
        Arc::new(Hwclock {}),
        Arc::downgrade(&misc_inode.get_arc()),
    );
    let mut lock = misc_inode.children.write();
    misc_inode.cache_all_subfile(&mut lock);
    lock.as_mut().unwrap().insert("rtc".to_string(), hwclock_dev);
    drop(lock);
}
fn init_tmp_directory() {
    match ROOT.mkdir("/tmp") {
        _ => {}
    }
}
fn init_proc_directory() {
    match ROOT.mkdir("/proc") {
        _ => {}
    }
    match ROOT.open("/proc/meminfo", OpenFlags::O_CREAT, false) {
        _ => {}
    }
    match ROOT.open("/proc/mounts", OpenFlags::O_CREAT, false) {
        _ => {}
    }
}
