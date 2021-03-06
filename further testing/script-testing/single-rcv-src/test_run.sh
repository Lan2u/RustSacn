#!/bin/bash

# This script is based on the scripts provided as part of the 2018/19 CS3102 Practical 1

TEST_NUM=$1
SRC_OUTPUT_PATH=$2
RCV_OUTPUT_PATH=$3
SRC_TEST_INPUT=$4
RCV_TEST_INPUT=$5
SRC_EXPECTED_OUTPUT=$6
RCV_EXPECTED_OUTPUT=$7
KILL_WAIT=$8 # How long should the testing program wait before killing both receiver and sender forcefully (seconds).

# The addresses of the machines used for the test. The addresses given here are host-names which are resolved by the the lab DNS.
# REMOTE_PC_2 is used for the receiver and REMOTE_PC for the sender.
REMOTE_PC=pc3-018-l
REMOTE_PC_2=pc3-026-l

# The default ACN port.
PORT=5568

# the current directory;
CURRENT_DIR=$(pwd)

# Startup a receiver on the second remote pc
echo "Running rcv at ${REMOTE_PC_2}"
ssh -n -f ${REMOTE_PC_2} "sh -c 'cd ${CURRENT_DIR}; nohup ./rcv.sh > ${RCV_OUTPUT_PATH} < ${RCV_TEST_INPUT} 2>/dev/null'"

# Give the receiver a chance to startup.
sleep 2

# Startup a sender on the first remote pc
echo "Running src at ${REMOTE_PC}"
ssh -n -f ${REMOTE_PC} "sh -c 'cd ${CURRENT_DIR}; nohup ./src.sh > ${SRC_OUTPUT_PATH} < ${SRC_TEST_INPUT} 2>/dev/null'"

# Wait to allow both processes the chance to run, this has no specific guarantee that they will 
#	as it depends on scheduling but it is hoped this will be long enough.
sleep ${KILL_WAIT}

# Kill stuff running on both PC's
ssh ${REMOTE_PC} fuser -k -n udp ${PORT}
ssh ${REMOTE_PC_2} fuser -k -n udp ${PORT}

# Give time for both processes to be killed properly.
sleep 2
