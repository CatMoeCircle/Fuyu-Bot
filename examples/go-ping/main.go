// ping 示例插件（Go）：收到 `/ping` 就回 `pong`。
//
// 结构上分两半：
//   - 本文件（Go）：真正的逻辑——解析更新 JSON、拼 sendMessage、打日志；
//   - plugin.c（C）：四个 ABI 导出符号的薄壳，把宿主的回调转成对 Go 函数的调用。
//
// 为什么不是全部用 `//export` 直接导出四个符号：PluginHandle 是按值返回的结构体，
// 由 C 侧构造最省事；也避免把宿主 VTable 的函数指针搬进 Go。
//
// 编译见 README（需要 C 工具链，cgo 的常规要求）。
package main

/*
#cgo CFLAGS: -I${SRCDIR}
#include <stdint.h>
#include <stdlib.h>
#include "fuyubot_plugin.h"

// 下面两个由 plugin.c 实现（Go 侧只声明）。
void fy_log(const char *message);
long long fy_invoke(const char *method_name, const char *params_json);
*/
import "C"

import (
	"encoding/json"
	"unsafe"
)

// 只关心这几个字段：updateNewMessage → message.content.text.text。
type update struct {
	Message struct {
		ChatID     int64 `json:"chat_id"`
		IsOutgoing bool  `json:"is_outgoing"`
		Content    struct {
			Type string `json:"@type"`
			Text struct {
				Text string `json:"text"`
			} `json:"text"`
		} `json:"content"`
	} `json:"message"`
}

//export go_on_update
func go_on_update(_ unsafe.Pointer, payload *C.char) C.int {
	if payload == nil {
		return 0
	}
	var u update
	if err := json.Unmarshal([]byte(C.GoString(payload)), &u); err != nil {
		return 0
	}
	if u.Message.IsOutgoing || u.Message.ChatID == 0 {
		return 0 // 自己发的消息必须跳过，否则会自激
	}
	if u.Message.Content.Type != "messageText" || u.Message.Content.Text.Text != "/ping" {
		return 0
	}

	// TDLib 只有 sendMessage：内容类型放 input_message_content，
	// 没有 Bot API 那种顶层 text / sendPhoto（AGENTS 约束 43）。
	params, err := json.Marshal(map[string]any{
		"chat_id": u.Message.ChatID,
		"input_message_content": map[string]any{
			"@type": "inputMessageText",
			"text": map[string]any{
				"@type":    "formattedText",
				"text":    "pong",
				"entities": []any{},
			},
		},
	})
	if err != nil {
		return 0
	}
	method := C.CString("sendMessage")
	body := C.CString(string(params))
	defer C.free(unsafe.Pointer(method))
	defer C.free(unsafe.Pointer(body))
	C.fy_invoke(method, body)

	msg := C.CString("go-ping 插件：回了 pong")
	defer C.free(unsafe.Pointer(msg))
	C.fy_log(msg)
	return 0
}

//export go_start
func go_start(_ *C.char) C.int {
	msg := C.CString("go-ping 插件：start()")
	defer C.free(unsafe.Pointer(msg))
	C.fy_log(msg)
	return 0
}

//export go_stop
func go_stop() {
	msg := C.CString("go-ping 插件：stop()")
	defer C.free(unsafe.Pointer(msg))
	C.fy_log(msg)
}

func main() {}
