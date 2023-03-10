## 问题发现

在有了轻量化C编译器后发现如果有一个文字处理程序就会更好，于是我们将目标指向了`busybox`的`vi`模块，通过这个可以实现在自己操作系统上写程序并编译，自产自销。

在支持`vi`的过程中，在解决完[不能输入](./vi不能输入bug.md)的问题后打算返回shell界面，但是出现下面这种怪现象：

![](./assets/vi-return%20fault-1.png)

我们观察到有三个现象

+ vi的那些字符在终端里没有完全清除
+ 文件被正确写入(紧接的一条命令是`cat <文件名>`，将文件内容打印出来)
+ bash没有出现回显(可以看到连`cat`都打印不出来)

## 问题原因

我们在网上查询相关资料后发现，tty的控制是通过`ioctl`系统调用进行查询修改`termio`结构体，里面有一个`lmode`变量，描述终端的相关状态，里面一个`ECHO`参数代表了是否需要内核对输入回显。

我们观察到在退出程序之后bash过来获取了一次`termio`，然后又写回了一次`termio`，发现写回那次没有将`ECHO`置位，bash也不主动输出。

于是追踪系统调用，发现在`vi`退出时有一个系统调用`ftruncate`未支持，导致`vi`进程崩溃退出，在逐一比对了Debian的strace，发现在这条系统调用后在退出进程前还输出了一些字符，这些应该是原来shell的内容，这点解释了为什么没有完全清除字符。

![](./assets/vi-return%20fault-2.png)

为什么bash没有回显，我们探索了一会，发现是当我们发现未支持系统调用时，让进程正常退出了。

```rust
_ => {
    error!(
        "Unsupported syscall:{} ({}), calling over arguments:",
        syscall_name(syscall_id),
        syscall_id
    );
    for i in 0..args.len() {
        error!("args[{}]: {:X}", i, args[i]);
    }
    info!("Exiting.");
    // 这句话是正常退出，告诉父进程：子进程的返回值为1
    sys_exit(1)
}
```

根据我们试验得出，bash在刚开始加载的时候将`ECHO`清位，在调用程序的时候会将`ECHO`先短暂复位，将主动权交给子进程，这个时候`ECHO`置位了就理应由内核将输入的字符打印回显，在进程正常退出之后bash会去先查看一次`termio`结构体，根据结构体判断终端相关的状态，是否需要打印回显(如果置位bash将代理内核回显，否则不回显)。

而在运行`vi`时`ECHO`位没有置位(由`vi`代理内核回显，这样才能够处理文字)，所以"正常"结束之后bash不回显。

于是我们将返回由正常结束变成了信号`SYSSIG`杀死，而由信号杀死的子进程，bash会忽略子进程所做的所有操作，包括`termio`结构体的修改，这样就不会影响到正常回显，不过依然无法解决没有完全清除字符(`vi`无法做完最后的字符操作)。

![](./assets/vi-return%20fault-3.png)