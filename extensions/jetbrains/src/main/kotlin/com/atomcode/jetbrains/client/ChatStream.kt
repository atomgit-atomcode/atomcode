package com.atomcode.jetbrains.client

import com.atomcode.jetbrains.protocol.ChatEvent
import com.atomcode.jetbrains.protocol.SseParser
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.flow
import kotlinx.coroutines.flow.flowOn
import java.net.http.HttpClient
import java.net.http.HttpRequest
import java.net.http.HttpResponse

/**
 * SSE 聊天流，以 Flow<ChatEvent> 的形式逐事件发射。
 */
class ChatStream(
    private val httpClient: HttpClient,
    private val httpRequest: HttpRequest
) {
    fun events(): Flow<ChatEvent> = flow {
        val response = httpClient.send(httpRequest, HttpResponse.BodyHandlers.ofLines())
        val parser = SseParser()

        for (line in response.body()) {
            parser.feed("$line\n").forEach { event ->
                emit(event)
            }
        }
        parser.flush().forEach { event ->
            emit(event)
        }
    }.flowOn(Dispatchers.IO)
}
