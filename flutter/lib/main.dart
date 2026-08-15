import 'dart:io';

import 'package:flutter/material.dart';
// ExternalLibrary 在 FRB 2.12 从 for_generated_io 入口导出
import 'package:flutter_rust_bridge/flutter_rust_bridge_for_generated_io.dart';

import 'src/rust/frb_generated.dart';
import 'widgets/hypermixx_screen.dart';

/// 桥库路径：env HYPERMIXX_BRIDGE_LIB > dart-define > 从工作目录向上找
/// `<仓库>/HyperMixx/target/release/libhypermixx_bridge.so`（bundle 深目录开发便利）。
String? _findBridgeLib() {
  const defineLib = String.fromEnvironment('HYPERMIXX_BRIDGE_LIB');
  if (defineLib.isNotEmpty) return defineLib;
  var d = Directory.current;
  for (var i = 0; i < 10; i++) {
    for (final rel in ['libhypermixx_bridge.so', 'HyperMixx/target/release/libhypermixx_bridge.so']) {
      final p = '${d.path}/$rel';
      if (File(p).existsSync()) return p;
    }
    if (d.parent.path == d.path) break;
    d = d.parent;
  }
  return null;
}

Future<void> main() async {
  final libPath = Platform.environment['HYPERMIXX_BRIDGE_LIB'] ?? _findBridgeLib();
  if (libPath == null) {
    debugPrint('未找到桥库 libhypermixx_bridge.so（设置 HYPERMIXX_BRIDGE_LIB 或先 cargo build -p hypermixx-bridge --release）');
    runApp(const HyperMixxApp(bridgeMissing: true));
    return;
  }
  debugPrint('加载桥库: $libPath');
  try {
    await RustLib.init(externalLibrary: ExternalLibrary.open(libPath));
    debugPrint('桥库加载成功');
  } catch (e) {
    debugPrint('桥库加载失败: $e');
  }
  runApp(const HyperMixxApp(bridgeMissing: false));
}

class HyperMixxApp extends StatelessWidget {
  const HyperMixxApp({super.key, this.bridgeMissing = false});

  /// 桥库缺失：显示提示屏（测试环境同样走这里）。
  final bool bridgeMissing;

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'HyperMixx',
      theme: ThemeData.dark(useMaterial3: true),
      home: bridgeMissing ? const _MissingScreen() : const HyperMixxScreen(),
    );
  }
}

class _MissingScreen extends StatelessWidget {
  const _MissingScreen();

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      backgroundColor: const Color(0xFF1A1E24),
      body: Center(
        child: Text(
          '未找到桥库 libhypermixx_bridge.so\n（设置 HYPERMIXX_BRIDGE_LIB 后重启）',
          textAlign: TextAlign.center,
          style: TextStyle(color: Colors.white.withValues(alpha: 0.7), fontSize: 16),
        ),
      ),
    );
  }
}
